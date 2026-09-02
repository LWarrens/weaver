use std::sync::Arc;
use std::fs;

use chrono::Utc;
use futures::stream::{self, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embeddings::{provider_from_env as embedding_provider_from_env, EmbeddingProvider};
use crate::error::Error;
use crate::llm::{provider_from_env, LlmProvider};
use crate::storage::SqliteStore;
use crate::tools::find_orphaned_code::{self, FindOrphanedCodeParams};
use crate::tools::generate_adr_draft::{self, GenerateAdrDraftParams};
use crate::tools::generate_adr_patch::{self, GenerateAdrPatchParams};
use crate::tools::record_episode::{self, RecordDecisionEpisodeParams, EpisodeDecision};
use crate::tools::json_utils::{parse_kv_lead, de as num_de};

// Phase-1 parallelism: overlap DB prefetch + embedding + LLM call across candidates.
// Set to 2 for local single-GPU models that queue requests; raise to 8+ for hosted APIs.
const CONCURRENCY: usize = 2;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SynthesizeAdrLeadsParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Optional path prefix to limit scanning.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Maximum number of leads to synthesize. Omit or null to process all orphaned files.
    #[serde(default, deserialize_with = "num_de::opt_u32")]
    pub limit: Option<u32>,
    /// Minimum confidence to keep a lead (0.0-1.0).
    #[serde(default, deserialize_with = "num_de::opt_f32")]
    pub min_confidence: Option<f32>,
    /// If true, only return artifacts but do not record episodes. Default: false.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Whether to record an episode for each synthesized lead. Default: true.
    #[serde(default = "default_true")]
    pub record_episode: bool,
    /// Episode source string, e.g. "synthetic:llm".
    #[serde(default = "default_episode_source")]
    pub episode_source: String,
    /// Jaccard similarity threshold for `affected_files` deduplication (0.0–1.0).
    /// Two leads whose file sets overlap at or above this threshold are treated as
    /// duplicates; the lower-confidence one is dropped. Default: 0.5. Set to 1.0
    /// to disable deduplication.
    #[serde(default, deserialize_with = "num_de::opt_f32")]
    pub dedup_threshold: Option<f32>,
}

fn default_true() -> bool {
    true
}

fn default_episode_source() -> String {
    "synthetic:llm".to_string()
}

fn jaccard_similarity(a: &[String], b: &[String]) -> f32 {
    let set_a: std::collections::HashSet<&str> = a.iter().map(String::as_str).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(String::as_str).collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.len() + set_b.len() - intersection;
    if union == 0 {
        return 1.0;
    }
    intersection as f32 / union as f32
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SynthesizedLead {
    pub id: String,
    pub title: String,
    pub markdown: String,
    pub patch: Option<String>,
    pub affected_files: Vec<String>,
    pub confidence: f32,
    pub episode_id: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SynthesizeAdrLeadsResult {
    pub leads: Vec<SynthesizedLead>,
    pub summary: SynthesizeSummary,
}

#[derive(Debug, Serialize)]
pub struct SynthesizeSummary {
    pub candidates_examined: u32,
    pub synthesized: u32,
    pub skipped: u32,
    pub deduplicated: u32,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: SynthesizeAdrLeadsParams,
) -> Result<SynthesizeAdrLeadsResult, Error> {
    // validate
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist or is not accessible: {}", params.repo_path),
    })?;

    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    // Ensure repository row exists (upsert)
    let repo = store.upsert_repository(repo_path_str, None).await?;

    // Default dry_run is false per new policy
    let dry_run = params.dry_run.unwrap_or(false);

    // --- Find orphaned code ------------------------------------------------
    let orphaned = find_orphaned_code::run(
        store,
        FindOrphanedCodeParams {
            repo_path: repo_path_str.to_string(),
            path_prefix: params.path_prefix.clone(),
        },
    )
    .await?;

    let mut warnings: Vec<String> = Vec::new();
    let mut leads: Vec<SynthesizedLead> = Vec::new();

    // LLM provider — required for synthesis.
    let provider: Arc<dyn LlmProvider> = match provider_from_env() {
        Some(p) => Arc::from(p),
        None => {
            warnings.push("no LLM provider configured; skipping synthesis".to_string());
            return Ok(SynthesizeAdrLeadsResult {
                leads,
                summary: SynthesizeSummary {
                    candidates_examined: 0,
                    synthesized: 0,
                    skipped: 0,
                    deduplicated: 0,
                    warnings,
                },
            });
        }
    };

    // Embedding provider for semantic decision lookup (optional — degrade gracefully).
    let emb_provider: Option<Arc<dyn EmbeddingProvider>> =
        embedding_provider_from_env().map(|p| Arc::from(p) as Arc<dyn EmbeddingProvider>);

    let limit = params.limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let mut candidates_examined: u32 = 0;
    let mut synthesized: u32 = 0;
    let mut skipped: u32 = 0;

    // Build candidates from orphaned files.
    let candidates: Vec<String> = orphaned
        .orphaned_files
        .into_iter()
        .map(|f| f.path)
        .collect();

    // ---------------------------------------------------------------------------
    // Phase 1 (parallel): file read + DB prefetch + embed query + LLM call.
    // ADR ID assignment and episode recording must remain serial (Phase 2).
    // ---------------------------------------------------------------------------

    #[derive(Debug, Deserialize)]
    struct LlmLead {
        title: Option<String>,
        observed_pattern: Option<String>,
        proposed_decision: Option<String>,
        affected_files: Option<Vec<String>>,
        confidence: Option<f32>,
        rationale: Option<String>,
    }

    struct CandidateCtx {
        path: String,
        symbols: Vec<(String, String)>,
        lead_json: String,
    }

    enum Phase1Out {
        SkipSilent,
        SkipWarn(String),
        Ready(CandidateCtx),
    }

    let repo_id: Uuid = repo.id;
    let repo_path_owned = repo_path_str.to_string();

    let phase1_results: Vec<Phase1Out> = stream::iter(candidates.into_iter().take(limit))
        .map(|path| {
            let store = store.clone();
            let provider = provider.clone();
            let emb_provider = emb_provider.clone();
            let repo_path_owned = repo_path_owned.clone();
            async move {
                let snippet = match fs::read_to_string(
                    std::path::Path::new(&repo_path_owned).join(&path),
                ) {
                    Ok(s) => {
                        let s = s.lines().take(40).collect::<Vec<_>>().join("\n");
                        if s.len() > 2000 { s[..2000].to_string() } else { s }
                    }
                    Err(_) => "".to_string(),
                };

                let symbols = store
                    .fetch_symbols_for_file(repo_id, &path)
                    .await
                    .unwrap_or_default();

                if symbols.is_empty() {
                    return Phase1Out::SkipSilent;
                }

                let recent_commits = store
                    .fetch_recent_commits_for_file(repo_id, &path, 3)
                    .await
                    .unwrap_or_default();

                let cochanged = store
                    .fetch_cochanged_files(repo_id, &path, 5)
                    .await
                    .unwrap_or_default();

                let related_decisions_json = if let Some(ref ep) = emb_provider {
                    let query = format!(
                        "{} {}",
                        path,
                        symbols.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(" ")
                    );
                    match ep.embed_chunked(&query, 512).await {
                        Ok(qvec) if !qvec.is_empty() => {
                            match store
                                .semantic_decisions_if_available(repo_id, Some(&qvec), None, 0.0)
                                .await
                            {
                                Ok(Some(decisions)) => {
                                    let top: Vec<_> = decisions.into_iter().take(3).map(|d| {
                                        let snip = d.text.chars().take(150).collect::<String>();
                                        serde_json::json!({"id": d.adr_id, "title": d.title, "summary": snip})
                                    }).collect();
                                    serde_json::to_string(&top).unwrap_or_else(|_| "[]".to_string())
                                }
                                _ => "[]".to_string(),
                            }
                        }
                        _ => "[]".to_string(),
                    }
                } else {
                    "[]".to_string()
                };

                let symbols_json = serde_json::to_string(
                    &symbols
                        .iter()
                        .map(|(n, k)| serde_json::json!({"name": n, "kind": k}))
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".to_string());

                let commits_json = serde_json::to_string(
                    &recent_commits
                        .iter()
                        .map(|(_, sha, author, msg, ts)| serde_json::json!({
                            "sha": &sha[..sha.len().min(8)],
                            "author": author,
                            "date": ts,
                            "message": msg.as_deref().unwrap_or("").lines().next().unwrap_or("")
                        }))
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".to_string());

                let cochanged_str = if cochanged.is_empty() {
                    "none recorded".to_string()
                } else {
                    cochanged
                        .iter()
                        .map(|(f, n)| format!("{f} ({n} commits)"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };

                let prompt = format!(
                    "You are an architecture analyst building a knowledge base of undocumented patterns.\n\
\n\
Context: leads are retrieved as architectural context when an LLM investigates code that has no \
formal ADR. A lead must describe what the code ALREADY DOES — not what it should do. \
Write leads as documentation, not recommendations.\n\
\n\
Existing documented decisions (do not duplicate these):\n\
{related_decisions_json}\n\
\n\
Files changed together with {path} (these likely share the same undocumented pattern):\n\
{cochanged_str}\n\
\n\
File: {path}\n\
Symbols: {symbols_json}\n\
Recent commits: {commits_json}\n\
---\n\
{snippet}\n\
\n\
Does this file participate in a cross-cutting pattern that spans at least one other file above \
and is NOT already covered by the documented decisions?\n\
\n\
Rules:\n\
- Describe what the code DOES, not what it should do. Use present tense: \"uses\", \"implements\", \
\"follows\" — not \"must\", \"should\", \"ought to\".\n\
- Only reference files you can infer from the context provided. Never invent file names.\n\
- FILES must include EVERY file from the co-changed list that participates in the \
same pattern — this list is used as a retrieval index, so completeness matters.\n\
- A valid lead spans 2 or more files. Single-file observations: confidence below 0.3.\n\
- Pattern already covered by a documented decision: confidence below 0.3.\n\
- Genuinely uncertain: confidence below 0.3.\n\
\n\
Reply with exactly these labelled fields and nothing else:\n\
TITLE: <concise pattern name>\n\
PATTERN: <what the code does, present tense, 1-3 sentences>\n\
RATIONALE: <why these files share this pattern>\n\
FILES: <comma-separated list of affected file paths>\n\
CONFIDENCE: <number 0.0-1.0>",
                    path = path,
                    related_decisions_json = related_decisions_json,
                    cochanged_str = cochanged_str,
                    symbols_json = symbols_json,
                    commits_json = commits_json,
                    snippet = snippet
                );

                match provider.generate(&prompt).await {
                    Ok(resp) => Phase1Out::Ready(CandidateCtx { path, symbols, lead_json: resp }),
                    Err(e) => Phase1Out::SkipWarn(format!("LLM generation failed for {path}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    // ---------------------------------------------------------------------------
    // Phase 1.5 (serial): dedup by affected_files Jaccard similarity.
    // Candidates are sorted by confidence desc so the strongest lead for a given
    // pattern survives. Candidates with no affected_files are left to Phase 2.
    // ---------------------------------------------------------------------------

    let dedup_threshold = params.dedup_threshold.unwrap_or(0.5).clamp(0.0, 1.0);
    let mut dup_indexes: std::collections::HashSet<usize> = std::collections::HashSet::new();

    if dedup_threshold < 1.0 {
        let mut parsed_candidates: Vec<(usize, f32, Vec<String>)> = Vec::new();
        for (i, result) in phase1_results.iter().enumerate() {
            if let Phase1Out::Ready(ctx) = result {
                if let Ok(lead) = serde_json::from_value::<LlmLead>(parse_kv_lead(&ctx.lead_json)) {
                    let confidence = lead.confidence.unwrap_or(0.5);
                    let affected = lead.affected_files.unwrap_or_default();
                    if !affected.is_empty() {
                        parsed_candidates.push((i, confidence, affected));
                    }
                }
            }
        }

        // Highest confidence first so the best lead wins each cluster.
        parsed_candidates
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut accepted: Vec<Vec<String>> = Vec::new();
        for (i, _confidence, affected_files) in parsed_candidates {
            let is_dup = accepted
                .iter()
                .any(|acc| jaccard_similarity(acc, &affected_files) >= dedup_threshold);
            if is_dup {
                dup_indexes.insert(i);
            } else {
                accepted.push(affected_files);
            }
        }
    }

    let mut deduplicated: u32 = 0;

    // ---------------------------------------------------------------------------
    // Phase 2 (serial): JSON parse → confidence filter → ADR draft → episode.
    // Serial here prevents generate_adr_draft from racing on next_adr_number.
    // ---------------------------------------------------------------------------

    for (i, result) in phase1_results.into_iter().enumerate() {
        candidates_examined += 1;
        if dup_indexes.contains(&i) {
            deduplicated += 1;
            skipped += 1;
            continue;
        }
        let ctx = match result {
            Phase1Out::SkipSilent => { skipped += 1; continue; }
            Phase1Out::SkipWarn(w) => { warnings.push(w); skipped += 1; continue; }
            Phase1Out::Ready(c) => c,
        };

        let CandidateCtx { path, symbols, lead_json } = ctx;

        let lead: LlmLead = match serde_json::from_value(parse_kv_lead(&lead_json)) {
            Ok(l) => l,
            Err(e) => {
                warnings.push(format!("could not parse KV lead for {}: {}", path, e));
                skipped += 1;
                continue;
            }
        };
        let lead = &lead;
        let confidence = lead.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
        let min_confidence = params.min_confidence.unwrap_or(0.3);
        if confidence < min_confidence {
            skipped += 1;
            continue;
        }

        let title = lead
            .title
            .clone()
            .unwrap_or_else(|| format!("Undocumented pattern in {}", path));
        let proposed_decision = lead
            .observed_pattern
            .clone()
            .or_else(|| lead.proposed_decision.clone())
            .unwrap_or_else(|| "Not specified".to_string());
        let affected_files = lead
            .affected_files
            .clone()
            .unwrap_or_else(|| vec![path.clone()]);

        let draft_res = generate_adr_draft::run(
            store,
            GenerateAdrDraftParams {
                repo_path: repo_path_str.to_string(),
                title: title.clone(),
                context: Some(lead.rationale.clone().unwrap_or_default()),
                proposed_decision: Some(proposed_decision.clone()),
                affected_files: affected_files.clone(),
            },
        )
        .await;

        let gen = match draft_res {
            Ok(d) => d,
            Err(e) => {
                warnings.push(format!("generate_adr_draft failed for {}: {}", path, e));
                skipped += 1;
                continue;
            }
        };

        let adr_path = format!("docs/adr/{}.md", gen.id);
        let patch_res = generate_adr_patch::run(GenerateAdrPatchParams {
            repo_path: repo_path_str.to_string(),
            adr_path: adr_path.clone(),
            draft: gen.markdown.clone(),
        })
        .await;

        let patch = match patch_res {
            Ok(p) => Some(p.patch),
            Err(e) => {
                warnings.push(format!("generate_adr_patch failed for {}: {}", path, e));
                None
            }
        };

        let mut episode_id: Option<String> = None;
        if params.record_episode && !dry_run {
            let occured = Utc::now().to_rfc3339();
            let content = format!(
                "Observed pattern in {}:\nTitle: {}\nPattern: {}\nRationale: {}\n",
                path, title, proposed_decision,
                lead.rationale.clone().unwrap_or_default()
            );
            let ep = RecordDecisionEpisodeParams {
                repo_path: repo_path_str.to_string(),
                source: params.episode_source.clone(),
                source_uri: None,
                occurred_at: occured,
                content,
                decisions: Some(vec![EpisodeDecision {
                    title: Some(title.clone()),
                    text: proposed_decision.clone(),
                    constraints: vec![],
                    affected_files: affected_files.clone(),
                    entities: symbols.iter().map(|(name, _)| name.clone()).collect(),
                }]),
                dedup_threshold: None,
            };

            match record_episode::run(store, ep).await {
                Ok(resp) => {
                    if let Some(ans) = resp.answer {
                        if let Some(id) = ans.strip_prefix("Stored episode ") {
                            episode_id = Some(id.to_string());
                        }
                    }
                }
                Err(e) => {
                    warnings.push(format!("record_episode failed for {}: {}", path, e));
                }
            }
        }

        leads.push(SynthesizedLead {
            id: gen.id,
            title,
            markdown: gen.markdown,
            patch,
            affected_files,
            confidence,
            episode_id,
            warnings: vec![],
        });
        synthesized += 1;
    }

    let summary = SynthesizeSummary {
        candidates_examined,
        synthesized,
        skipped,
        deduplicated,
        warnings: warnings.clone(),
    };

    Ok(SynthesizeAdrLeadsResult { leads, summary })
}

// Use shared tolerant extractor in `json_utils.rs`.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_identical_sets() {
        let a = vec!["a.rs".to_string(), "b.rs".to_string()];
        assert_eq!(jaccard_similarity(&a, &a), 1.0);
    }

    #[test]
    fn jaccard_disjoint_sets() {
        let a = vec!["a.rs".to_string()];
        let b = vec!["b.rs".to_string()];
        assert_eq!(jaccard_similarity(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a = vec!["a.rs".to_string(), "b.rs".to_string()];
        let b = vec!["b.rs".to_string(), "c.rs".to_string()];
        // intersection=1, union=3 → 1/3 ≈ 0.333
        let sim = jaccard_similarity(&a, &b);
        assert!((sim - 1.0 / 3.0).abs() < 1e-5, "got {sim}");
    }

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    #[tokio::test]
    async fn scaffold_returns_summary() {
        let store = test_store().await;

        let dir = tempfile::tempdir().unwrap();
        let params = SynthesizeAdrLeadsParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            path_prefix: None,
            limit: Some(3),
            min_confidence: Some(0.5),
            dry_run: Some(true),
            record_episode: true,
            episode_source: "synthetic:llm".to_string(),
            dedup_threshold: None,
        };

        let res = run(&store, params).await.expect("run");
        assert_eq!(res.summary.synthesized, 0);
    }

    #[tokio::test]
    async fn synth_with_mock_llm_returns_draft() {
        let _guard = crate::env_guard();

        // configure mock LLM
        std::env::set_var("WEAVER_LLM_PROVIDER", "mock");
        std::env::set_var(
            "WEAVER_LLM_RESPONSE",
            r#"[
  {
    "title": "Consolidate logging",
    "proposed_decision": "Use a shared structured logging approach across services.",
    "affected_files": ["src/log.rs"],
    "confidence": 0.8,
    "rationale": "Inconsistent logging observed"
  }
]"#,
        );

        let store = test_store().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/log.rs"), "pub fn log(msg: &str) { println!(\"{}\", msg); }\n").unwrap();

        // ingest files so orphan detection can see them
        crate::tools::ingest_symbols::run(
            &store,
            crate::tools::ingest_symbols::IngestSymbolsParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await
        .expect("ingest");

        let params = SynthesizeAdrLeadsParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            path_prefix: None,
            limit: Some(2),
            min_confidence: Some(0.5),
            dry_run: Some(true),
            record_episode: false,
            episode_source: "synthetic:llm".to_string(),
            dedup_threshold: None,
        };

        let res = run(&store, params).await.expect("run");
        assert!(res.summary.synthesized >= 1, "expected at least one lead");
        assert!(!res.leads.is_empty());

        // leave environment as-is (tests may run in parallel and manage env themselves)
    }
}
