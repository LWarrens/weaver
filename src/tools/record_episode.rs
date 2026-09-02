use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::entities::{
    AnchorIdentity, AnchorSource, ArchResponse, Constraint, ConstraintSummary, Decision,
    DecisionCodeLink, DecisionSummary, EvidenceGrade, LinkSource, LinkType, Locator, TemporalContext,
};
use crate::domain::{anchors, claims};
use crate::embeddings::{cosine_similarity, pack_f32, provider_from_env, unpack_f32};
use crate::error::Error;
use crate::storage::sqlite::TemporalEdge;
use crate::storage::SqliteStore;
use crate::tools::json_utils::extract_json_array;

const VALID_FACT_RELATIONS: &[&str] = &[
    "must_use",
    "must_not_use",
    "must_call",
    "must_not_call",
    "requires",
    "prohibits",
    "replaces",
    "depends_on",
    "conflicts_with",
    "applies_to",
];

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordDecisionEpisodeParams {
    /// Absolute path to the repository root; used to upsert the repository record.
    pub repo_path: String,
    /// Who emitted the episode (e.g., "github:pr/123", "meeting:design-2026-05-03").
    pub source: String,
    /// Optional URI for the source (e.g., PR URL, file link).
    pub source_uri: Option<String>,
    /// ISO-8601 timestamp when the episode occurred.
    pub occurred_at: String,
    /// Raw textual content of the episode.
    pub content: String,
    /// Optional structured decisions extracted by the caller/agent.
    #[serde(default)]
    pub decisions: Option<Vec<EpisodeDecision>>,
    /// Cosine-similarity threshold (0.0-1.0) at or above which an incoming
    /// decision is merged into an existing open decision instead of inserted.
    /// Defaults to 0.9. At 1.0, only normalized-exact text matches merge.
    #[serde(default)]
    pub dedup_threshold: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EpisodeDecision {
    /// Optional short title for the decision.
    #[serde(default)]
    pub title: Option<String>,
    /// The decision text (what was decided).
    pub text: String,
    /// Optional constraints derived from the decision.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Optional affected files mentioned by the agent that relate to the decision.
    #[serde(default)]
    pub affected_files: Vec<String>,
    /// Optional canonical entity names (services, modules, components) involved in this decision.
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ExtractedFact {
    subject: String,
    relation: String,
    object: String,
    confidence: f64,
    #[serde(default)]
    temporal_hint: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Debug, Clone)]
struct TemporalRange {
    valid_from: String,
    valid_to: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: RecordDecisionEpisodeParams,
) -> Result<ArchResponse, Error> {
    // Resolve repository record — canonicalize so all tools agree on the stored path.
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!("path does not exist: {}", params.repo_path),
        })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let now = Utc::now().to_rfc3339();
    let embedding_provider = provider_from_env();

    // Insert episode row
    let episode_id = Uuid::new_v4();
    store
        .insert_episode(
            &episode_id,
            repo.id,
            &params.source,
            params.source_uri.as_deref(),
            &params.content,
            &params.occurred_at,
            &now,
        )
        .await?;

    if let Some(ref provider) = embedding_provider {
        if let Ok(vec) = provider.embed_chunked(&params.content, 512).await {
            if !vec.is_empty() {
                let blob = pack_f32(&vec);
                let _ = store.update_episode_embedding(episode_id, &blob).await;
            }
        }
    }

    let mut warnings = Vec::new();
    let extracted_facts = match extract_episode_facts(&params.content).await {
        Ok(facts) => facts,
        Err(FactExtractionSkip::NotConfigured) => {
            warnings.push("no LLM provider configured; facts not extracted".to_string());
            Vec::new()
        }
        Err(FactExtractionSkip::Failed(message)) => {
            warnings.push(format!("fact extraction failed: {}", message));
            Vec::new()
        }
    };
    let mut facts_extracted = 0;
    for fact in extracted_facts {
        if !VALID_FACT_RELATIONS.contains(&fact.relation.as_str()) {
            warnings.push(format!(
                "ignored extracted fact with unsupported relation '{}'",
                fact.relation
            ));
            continue;
        }

        let temporal = parse_temporal_hint(fact.temporal_hint.as_deref(), &params.occurred_at);
        let edge = TemporalEdge {
            id: Uuid::new_v4(),
            edge_type: fact.relation,
            source_id: fact.subject,
            source_type: "fact_subject".to_string(),
            target_id: fact.object,
            target_type: "fact_object".to_string(),
            valid_from: temporal.valid_from,
            valid_to: temporal.valid_to,
            ingested_at: now.clone(),
            confidence: fact.confidence.clamp(0.0, 1.0),
            evidence_refs: vec![episode_id],
        };
        store.insert_temporal_edge(&edge).await?;
        facts_extracted += 1;
    }

    let mut decisions_out: Vec<DecisionSummary> = Vec::new();
    let mut constraints_out: Vec<ConstraintSummary> = Vec::new();
    let mut decisions_merged = 0usize;

    if let Some(decisions) = params.decisions {
        let dedup_threshold = params.dedup_threshold.unwrap_or(0.9).clamp(0.0, 1.0);

        // Known open decisions for cross-episode entity resolution: normalized
        // text always, embeddings when stored.
        let mut known: Vec<KnownDecision> = store
            .fetch_decisions_with_embeddings(repo.id, None)
            .await?
            .into_iter()
            .map(|(summary, blob)| KnownDecision {
                normalized: normalize_decision_text(&summary.text),
                embedding: blob.map(|b| unpack_f32(&b)).filter(|v| !v.is_empty()),
                summary,
            })
            .collect();

        for d in decisions {
            let title = d
                .title
                .clone()
                .unwrap_or_else(|| "Episode decision".to_string());
            let source_uri = format!("episode:{}", episode_id);

            // Embed once up front; reused for dedup and for the stored embedding.
            let text_embedding = match embedding_provider {
                Some(ref provider) => provider
                    .embed_chunked(&d.text, 512)
                    .await
                    .ok()
                    .filter(|v| !v.is_empty()),
                None => None,
            };

            let normalized = normalize_decision_text(&d.text);
            let duplicate = find_duplicate_decision(
                &known,
                &normalized,
                text_embedding.as_deref(),
                dedup_threshold,
            );

            // Entity resolution: an incoming decision matching an existing open
            // decision (normalized-exact text, or cosine similarity >=
            // dedup_threshold) is merged into it instead of inserted. The
            // episode is linked through a `supports` Episode -> Decision edge
            // carrying the match similarity, so the merge stays auditable and
            // distinguishable from accepted architectural truth.
            let (decision_id, merged_summary) = match duplicate {
                Some((idx, similarity)) => {
                    let existing = known[idx].summary.clone();
                    let existing_id = Uuid::parse_str(&existing.id).map_err(|e| {
                        Error::Other(anyhow::anyhow!("invalid decision id in store: {e}"))
                    })?;

                    let supports_edge = TemporalEdge {
                        id: Uuid::new_v4(),
                        edge_type: "supports".to_string(),
                        source_id: episode_id.to_string(),
                        source_type: "episode".to_string(),
                        target_id: existing.id.clone(),
                        target_type: "decision".to_string(),
                        valid_from: now.clone(),
                        valid_to: None,
                        ingested_at: now.clone(),
                        confidence: similarity as f64,
                        evidence_refs: vec![episode_id],
                    };
                    store.insert_temporal_edge_if_absent(&supports_edge).await?;

                    warnings.push(format!(
                        "episode decision '{}' merged into existing decision {} (similarity {:.2})",
                        title, existing.id, similarity
                    ));
                    decisions_merged += 1;
                    (existing_id, Some(existing))
                }
                None => {
                    let decision = Decision {
                        id: Uuid::new_v4(),
                        title: Some(title.clone()),
                        adr_id: None,
                        episode_id: Some(episode_id),
                        text: d.text.clone(),
                        source_uri: source_uri.clone(),
                        valid_from: now.clone(),
                        valid_to: None,
                        ingested_at: now.clone(),
                        source_time: Some(params.occurred_at.clone()),
                        confidence: 1.0,
                        evidence_refs: vec![],
                    };

                    let decision_id = decision.id;
                    store.insert_decision(&decision).await?;

                    if let Some(ref vec) = text_embedding {
                        let blob = pack_f32(vec);
                        let _ = store.update_decision_embedding(decision_id, &blob).await;
                    }

                    (decision_id, None)
                }
            };

            // Claim + episode-content anchor for this decision (grade `partial`:
            // agent/LLM supplied, no source offsets). Merged decisions reuse the
            // surviving decision's existing claim.
            let decision_claim_id = episode_decision_claim_id(
                store,
                repo.id,
                decision_id,
                &d.text,
                episode_id,
                &params.content,
                &now,
                &params.occurred_at,
            )
            .await?;

            let summary = match &merged_summary {
                Some(existing) => existing.clone(),
                None => DecisionSummary {
                    id: decision_id.to_string(),
                    adr_id: source_uri.clone(),
                    episode_id: Some(episode_id.to_string()),
                    title: title.clone(),
                    status: "episode".to_string(),
                    text: d.text.clone(),
                    valid_from: now.clone(),
                    valid_to: None,
                    confidence: 1.0,
                },
            };

            // On merge, guard against re-attaching constraints and file links
            // the existing decision already carries.
            let existing_constraint_texts: std::collections::HashSet<String> =
                if merged_summary.is_some() {
                    store
                        .find_constraints_for_decisions(&[decision_id.to_string()])
                        .await?
                        .into_iter()
                        .map(|c| normalize_decision_text(&c.text))
                        .collect()
                } else {
                    Default::default()
                };
            let already_linked: std::collections::HashSet<String> = if merged_summary.is_some() {
                store
                    .file_paths_linked_to_decision(&decision_id.to_string())
                    .await?
                    .into_iter()
                    .collect()
            } else {
                Default::default()
            };

            // Constraints
            for ctext in d.constraints {
                if existing_constraint_texts.contains(&normalize_decision_text(&ctext)) {
                    continue;
                }
                let constraint = Constraint {
                    id: Uuid::new_v4(),
                    decision_id,
                    text: ctext.clone(),
                    source_uri: source_uri.clone(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    source_time: Some(params.occurred_at.clone()),
                    confidence: 1.0,
                    evidence_refs: vec![],
                };

                let constraint_id = constraint.id;
                store.insert_constraint(&constraint).await?;

                // Claim + episode-content anchor for the constraint.
                let c_identity = AnchorIdentity {
                    source_kind: AnchorSource::Episode,
                    source_uri: format!("episode:{episode_id}"),
                    subpath: String::new(),
                };
                let c_claim = claims::constraint_claim(
                    repo.id,
                    constraint_id,
                    &ctext,
                    EvidenceGrade::Partial,
                    vec![c_identity.clone()],
                    &now,
                    Some(params.occurred_at.clone()),
                    1.0,
                );
                store.insert_claim(&c_claim).await?;
                store
                    .insert_evidence_anchor(&anchors::build_anchor(
                        repo.id,
                        c_claim.id,
                        c_identity,
                        Locator::Chars {
                            start: 0,
                            end: params.content.chars().count(),
                        },
                        params.content.clone(),
                        None,
                        None,
                        &now,
                        Some(params.occurred_at.clone()),
                    ))
                    .await?;

                // imposes edge: Decision -> Constraint (mirrors sync_adrs emission)
                let imposes_edge = TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "imposes".to_string(),
                    source_id: decision_id.to_string(),
                    source_type: "decision".to_string(),
                    target_id: constraint_id.to_string(),
                    target_type: "constraint".to_string(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    confidence: 1.0,
                    evidence_refs: vec![episode_id],
                };
                store.insert_temporal_edge(&imposes_edge).await?;

                if let Some(ref provider) = embedding_provider {
                    if let Ok(vec) = provider.embed_chunked(&ctext, 512).await {
                        if !vec.is_empty() {
                            let blob = pack_f32(&vec);
                            let _ = store.update_constraint_embedding(constraint_id, &blob).await;
                        }
                    }
                }

                constraints_out.push(ConstraintSummary {
                    id: constraint.id.to_string(),
                    decision_id: decision_id.to_string(),
                    text: constraint.text.clone(),
                    confidence: constraint.confidence,
                });
            }

            // Decision <-> file links
            for fp in &d.affected_files {
                if already_linked.contains(fp) {
                    continue;
                }
                let link = DecisionCodeLink {
                    id: Uuid::new_v4(),
                    decision_id,
                    file_path: fp.clone(),
                    symbol: None,
                    link_type: LinkType::Mentions,
                    link_source: LinkSource::Inferred,
                    confidence: 0.7,
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    evidence_refs: vec![],
                };
                store.insert_decision_code_link(&link).await?;
            }

            // Resolve and link entities mentioned in this decision
            for entity_name in &d.entities {
                let entity = store
                    .upsert_entity_node(repo.id, entity_name, None, 0.7, &now)
                    .await?;
                let edge = TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "mentions".to_string(),
                    source_id: decision_id.to_string(),
                    source_type: "decision".to_string(),
                    target_id: entity.id.to_string(),
                    target_type: "entity".to_string(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    confidence: 0.9,
                    evidence_refs: vec![episode_id],
                };
                store.insert_temporal_edge_if_absent(&edge).await?;

                // Where the entity resolves to a live symbol, anchor the
                // decision claim to that symbol's span so freshness tracks the
                // code, not just the immutable episode text
                // (docs/DESIGN-claims-and-freshness.md).
                if let Ok(Some((path, start, end))) =
                    store.resolve_symbol_span(repo.id, entity_name).await
                {
                    let span_text = std::fs::read_to_string(repo_path.join(&path))
                        .ok()
                        .map(|src| {
                            src.lines()
                                .skip(start.saturating_sub(1) as usize)
                                .take((end.saturating_sub(start) + 1) as usize)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !span_text.trim().is_empty() {
                        let sym_identity = AnchorIdentity {
                            source_kind: AnchorSource::Symbol,
                            source_uri: path.clone(),
                            subpath: entity_name.clone(),
                        };
                        store
                            .insert_evidence_anchor(&anchors::build_anchor(
                                repo.id,
                                decision_claim_id,
                                sym_identity,
                                Locator::SymbolQn {
                                    qn: entity_name.clone(),
                                },
                                span_text,
                                None,
                                None,
                                &now,
                                Some(params.occurred_at.clone()),
                            ))
                            .await?;
                    }
                }
            }

            if merged_summary.is_none() {
                known.push(KnownDecision {
                    normalized,
                    embedding: text_embedding.clone(),
                    summary: summary.clone(),
                });
            }
            decisions_out.push(summary);
        }
    }

    let mut resp = ArchResponse::empty();
    resp.answer = Some(if decisions_merged > 0 {
        format!(
            "Stored episode {} ({} decision(s) merged into existing decisions)",
            episode_id, decisions_merged
        )
    } else {
        format!("Stored episode {}", episode_id)
    });
    resp.decisions = decisions_out;
    resp.constraints = constraints_out;
    resp.facts_extracted = facts_extracted;
    resp.warnings = warnings;
    resp.temporal_context = TemporalContext {
        valid_at: None,
        ingested_at: Some(now),
    };
    resp.confidence = 1.0;

    Ok(resp)
}

/// Claim id for an episode-sourced decision: the existing claim if the decision
/// already has one (a merge target), otherwise a fresh `partial`-grade claim
/// anchored to the whole episode content.
#[allow(clippy::too_many_arguments)]
async fn episode_decision_claim_id(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    decision_id: Uuid,
    decision_text: &str,
    episode_id: Uuid,
    content: &str,
    now: &str,
    occurred_at: &str,
) -> Result<Uuid, Error> {
    if let Some(existing) = store
        .claims_for_subject("decision", decision_id)
        .await?
        .into_iter()
        .next()
    {
        return Ok(existing.id);
    }
    let identity = AnchorIdentity {
        source_kind: AnchorSource::Episode,
        source_uri: format!("episode:{episode_id}"),
        subpath: String::new(),
    };
    let claim = claims::decision_claim(
        repo_id,
        decision_id,
        decision_text,
        EvidenceGrade::Partial,
        vec![identity.clone()],
        now,
        Some(occurred_at.to_string()),
        1.0,
    );
    store.insert_claim(&claim).await?;
    store
        .insert_evidence_anchor(&anchors::build_anchor(
            repo_id,
            claim.id,
            identity,
            Locator::Chars {
                start: 0,
                end: content.chars().count(),
            },
            content.to_string(),
            None,
            None,
            now,
            Some(occurred_at.to_string()),
        ))
        .await?;
    Ok(claim.id)
}

// ---------------------------------------------------------------------------
// Decision dedup (entity resolution)
// ---------------------------------------------------------------------------

struct KnownDecision {
    summary: DecisionSummary,
    normalized: String,
    embedding: Option<Vec<f32>>,
}

/// Lowercase and collapse whitespace so trivially reformatted text matches.
fn normalize_decision_text(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Best duplicate among known decisions: a normalized-exact text match wins
/// outright (similarity 1.0); otherwise the highest cosine similarity at or
/// above `threshold` when both sides have embeddings.
fn find_duplicate_decision(
    known: &[KnownDecision],
    normalized: &str,
    embedding: Option<&[f32]>,
    threshold: f32,
) -> Option<(usize, f32)> {
    if let Some(idx) = known.iter().position(|k| k.normalized == normalized) {
        return Some((idx, 1.0));
    }
    let embedding = embedding?;
    let mut best: Option<(usize, f32)> = None;
    for (idx, k) in known.iter().enumerate() {
        let Some(e) = &k.embedding else { continue };
        let sim = cosine_similarity(embedding, e);
        if sim >= threshold && best.map_or(true, |(_, b)| sim > b) {
            best = Some((idx, sim));
        }
    }
    best
}

#[derive(Debug)]
enum FactExtractionSkip {
    NotConfigured,
    Failed(String),
}

async fn extract_episode_facts(content: &str) -> Result<Vec<ExtractedFact>, FactExtractionSkip> {
    let provider = match crate::llm::provider_from_env() {
        Some(p) => p,
        None => return Err(FactExtractionSkip::NotConfigured),
    };
    let prompt = fact_extraction_prompt(content);
    let response = provider
        .generate(&prompt)
        .await
        .map_err(|e| FactExtractionSkip::Failed(e.to_string()))?;
    serde_json::from_str(&extract_json_array(&response))
        .map_err(|e| FactExtractionSkip::Failed(format!("invalid fact JSON: {}", e)))
}

fn fact_extraction_prompt(content: &str) -> String {
    format!(
        "Extract architectural decisions and constraints from the text below. \
Return a JSON array where each element has: \
{{\"subject\": string, \"relation\": one of [must_use,must_not_use,must_call,must_not_call,requires,prohibits,replaces,depends_on,conflicts_with,applies_to], \
\"object\": string, \"confidence\": number 0.0-1.0, \"temporal_hint\": string or null, \"rationale\": string or null}}. \
Omit observations and questions. Return [] if nothing qualifies.\n{}",
        content
    )
}

// Use shared tolerant extractor in `json_utils.rs`.

fn parse_temporal_hint(hint: Option<&str>, occurred_at: &str) -> TemporalRange {
    let valid_to = match hint.map(|h| h.trim().to_ascii_lowercase()) {
        Some(h) if h == "this sprint" => DateTime::parse_from_rfc3339(occurred_at)
            .ok()
            .map(|dt| (dt.with_timezone(&Utc) + Duration::days(14)).to_rfc3339()),
        _ => None,
    };

    TemporalRange {
        valid_from: occurred_at.to_string(),
        valid_to,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::TemporalMode;

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_episode_decision_merges_into_existing() {
        let _guard = crate::env_guard();
        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_LLM_RESPONSE");
        std::env::remove_var("WEAVER_EMBEDDING_PROVIDER");
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        let first = run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "meeting:day-1".to_string(),
                source_uri: None,
                occurred_at: "2026-06-01T09:00:00Z".to_string(),
                content: "First discussion.".to_string(),
                decisions: Some(vec![EpisodeDecision {
                    title: Some("Use event sourcing".to_string()),
                    text: "We will use event sourcing for audit history.".to_string(),
                    constraints: vec!["All writes must emit an event.".to_string()],
                    affected_files: vec!["src/events.rs".to_string()],
                    entities: vec![],
                }]),
                dedup_threshold: None,
            },
        )
        .await
        .expect("first episode");
        let original_id = first.decisions[0].id.clone();

        // Same decision, trivially reformatted, plus one duplicate and one
        // new constraint, one duplicate and one new file link.
        let second = run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "github:pr/9".to_string(),
                source_uri: None,
                occurred_at: "2026-06-02T09:00:00Z".to_string(),
                content: "Follow-up discussion.".to_string(),
                decisions: Some(vec![EpisodeDecision {
                    title: Some("Event sourcing (again)".to_string()),
                    text: "  We will use   Event Sourcing for audit history. ".to_string(),
                    constraints: vec![
                        "All writes must emit an event.".to_string(),
                        "Events must be immutable.".to_string(),
                    ],
                    affected_files: vec![
                        "src/events.rs".to_string(),
                        "src/replay.rs".to_string(),
                    ],
                    entities: vec![],
                }]),
                dedup_threshold: None,
            },
        )
        .await
        .expect("second episode");

        // Merged into the existing decision, not inserted
        assert_eq!(second.decisions.len(), 1);
        assert_eq!(second.decisions[0].id, original_id);
        assert!(
            second.answer.as_deref().unwrap().contains("merged"),
            "answer: {:?}",
            second.answer
        );
        assert!(
            second.warnings.iter().any(|w| w.contains("merged into existing decision")),
            "warnings: {:?}",
            second.warnings
        );

        let repo = store.upsert_repository(&repo_path, None).await.expect("repo");
        let all = store.list_all_decisions(repo.id, None).await.expect("all");
        assert_eq!(all.len(), 1, "no duplicate decision row");

        // Constraint dedup: original + the genuinely new one
        let constraints = store
            .find_constraints_for_decisions(&[original_id.clone()])
            .await
            .expect("constraints");
        assert_eq!(constraints.len(), 2, "constraints: {:?}", constraints);

        // File-link dedup: original + the new one
        let files = store
            .file_paths_linked_to_decision(&original_id)
            .await
            .expect("files");
        assert_eq!(files.len(), 2, "files: {:?}", files);

        // supports edge Episode -> Decision recorded with the merge evidence
        assert_eq!(
            store.count_open_temporal_edges_of_type("supports").await.unwrap(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intra_episode_duplicate_decisions_merge() {
        let _guard = crate::env_guard();
        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_EMBEDDING_PROVIDER");
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        let result = run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "meeting:dup".to_string(),
                source_uri: None,
                occurred_at: "2026-06-01T09:00:00Z".to_string(),
                content: "Repeated decision in one episode.".to_string(),
                decisions: Some(vec![
                    EpisodeDecision {
                        title: None,
                        text: "Cache reads through Redis.".to_string(),
                        constraints: vec![],
                        affected_files: vec![],
                        entities: vec![],
                    },
                    EpisodeDecision {
                        title: None,
                        text: "cache reads through redis.".to_string(),
                        constraints: vec![],
                        affected_files: vec![],
                        entities: vec![],
                    },
                ]),
                dedup_threshold: None,
            },
        )
        .await
        .expect("episode");

        assert_eq!(result.decisions.len(), 2, "both reported");
        assert_eq!(result.decisions[0].id, result.decisions[1].id, "same decision");

        let repo = store.upsert_repository(&repo_path, None).await.expect("repo");
        let all = store.list_all_decisions(repo.id, None).await.expect("all");
        assert_eq!(all.len(), 1, "only one row stored");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn record_decision_episode_does_not_create_adr_document() {
        let _guard = crate::env_guard();
        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_LLM_RESPONSE");
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        let result = run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "github:pr/4".to_string(),
                source_uri: Some("https://example.invalid/pr/4".to_string()),
                occurred_at: "2026-05-05T09:00:00Z".to_string(),
                content: "We decided episode decisions must not create ADR rows.".to_string(),
                decisions: Some(vec![EpisodeDecision {
                    title: Some("Store episode decisions directly".to_string()),
                    text: "Episode decisions link directly to their source episode.".to_string(),
                    constraints: vec![
                        "Do not create synthetic ADR documents for episodes.".to_string()
                    ],
                    affected_files: vec!["src/storage/sqlite.rs".to_string()],
                    entities: vec![],
                }]),
                dedup_threshold: None,
            },
        )
        .await
        .expect("record episode");

        let repo = store
            .upsert_repository(&repo_path, None)
            .await
            .expect("repo");
        let adrs = store.list_current_adrs(repo.id).await.expect("adrs");
        assert!(
            adrs.is_empty(),
            "episode recording must not create ADR documents"
        );

        assert_eq!(result.decisions.len(), 1);
        let decision = &result.decisions[0];
        assert_eq!(decision.title, "Store episode decisions directly");
        assert_eq!(decision.status, "episode");
        assert!(
            decision.episode_id.is_some(),
            "decision summary should expose the linked episode"
        );
        assert_eq!(
            decision.adr_id,
            format!("episode:{}", decision.episode_id.as_ref().unwrap())
        );

        let linked = store
            .find_decisions_for_file(repo.id, "src/storage/sqlite.rs", None, TemporalMode::Event)
            .await
            .expect("linked decisions");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].episode_id, decision.episode_id);

        // Decision → Constraint imposes edge, evidenced by the episode
        let episode_id =
            Uuid::parse_str(decision.episode_id.as_ref().unwrap()).expect("episode uuid");
        let edges = store
            .temporal_edges_for_evidence(episode_id)
            .await
            .expect("edges");
        let imposes: Vec<_> = edges.iter().filter(|e| e.edge_type == "imposes").collect();
        assert_eq!(imposes.len(), 1, "one imposes edge per constraint");
        assert_eq!(imposes[0].source_id, decision.id);
        assert_eq!(imposes[0].source_type, "decision");
        assert_eq!(imposes[0].target_type, "constraint");
        assert_eq!(imposes[0].target_id, result.constraints[0].id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn record_decision_episode_extracts_facts_with_mock_llm() {
        let _guard = crate::env_guard();
        std::env::set_var("WEAVER_LLM_PROVIDER", "mock");
        std::env::set_var(
            "WEAVER_LLM_RESPONSE",
            r#"[
              {
                "subject": "auth service",
                "relation": "must_not_call",
                "object": "payment service directly",
                "confidence": 0.9,
                "temporal_hint": "this sprint",
                "rationale": "avoid bounded context coupling"
              }
            ]"#,
        );

        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        let result = run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path,
                source: "meeting:auth".to_string(),
                source_uri: None,
                occurred_at: "2026-05-05T09:00:00Z".to_string(),
                content: "Auth must not call payments directly this sprint.".to_string(),
                decisions: None,
                dedup_threshold: None,
            },
        )
        .await
        .expect("record episode");

        assert_eq!(result.facts_extracted, 1);
        assert!(result.warnings.is_empty(), "warnings={:?}", result.warnings);
        let episode_id = Uuid::parse_str(
            result
                .answer
                .as_deref()
                .expect("answer")
                .trim_start_matches("Stored episode "),
        )
        .expect("episode id");
        let edges = store
            .temporal_edges_for_evidence(episode_id)
            .await
            .expect("edges");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_type, "must_not_call");
        assert_eq!(edges[0].source_id, "auth service");
        assert_eq!(edges[0].target_id, "payment service directly");
        assert_eq!(edges[0].confidence, 0.9);
        assert_eq!(edges[0].valid_from, "2026-05-05T09:00:00Z");
        assert_eq!(
            edges[0].valid_to.as_deref(),
            Some("2026-05-19T09:00:00+00:00")
        );
        assert_eq!(edges[0].evidence_refs, vec![episode_id]);

        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_LLM_RESPONSE");
    }
}
