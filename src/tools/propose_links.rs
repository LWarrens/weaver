use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::embeddings::{cosine_similarity, unpack_f32};
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeLinksParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// ADR ID (e.g. "ADR-0042") or decision UUID to generate candidates for.
    pub target: String,
    /// Maximum candidates to return per entity type. Default: 5.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LinkCandidate {
    /// Entity type: "decision", "commit", "symbol", or "route".
    pub entity_type: String,
    pub entity_id: String,
    /// Human-readable label (commit short message, symbol name, ADR title, route path).
    pub title: String,
    /// Confidence score in [0.0, 1.0]. Always explicit; never promoted silently.
    pub confidence: f64,
    /// Human-readable explanation of why this candidate was suggested.
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ProposeLinksResult {
    pub target_id: String,
    pub target_adr_id: String,
    pub target_title: String,
    pub candidates: Vec<LinkCandidate>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: ProposeLinksParams,
) -> Result<ProposeLinksResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let limit = params.limit.unwrap_or(5);
    let mut warnings: Vec<String> = vec![];

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // ------------------------------------------------------------------
    // Resolve target → (decision_id, adr_id_str, title, body_text, file_mentions, embedding)
    // ------------------------------------------------------------------
    let (target_decision_id, target_adr_id, target_title, target_text, file_mentions, target_embedding) =
        resolve_target(store, repo.id, &params.target, &mut warnings).await?;

    // ------------------------------------------------------------------
    // Gather candidates from each source
    // ------------------------------------------------------------------
    let terms = extract_terms(&target_text);

    let mut candidates: Vec<LinkCandidate> = vec![];

    // 1. Related decisions (excluding self)
    {
        let decision_candidates = suggest_decisions(
            store,
            repo.id,
            &target_decision_id,
            &target_text,
            target_embedding.as_deref(),
            &terms,
            limit,
        )
        .await?;
        candidates.extend(decision_candidates);
    }

    // 2. Related commits
    {
        let commit_candidates = suggest_commits(
            store,
            repo.id,
            &target_text,
            target_embedding.as_deref(),
            &terms,
            limit,
            &mut warnings,
        )
        .await?;
        candidates.extend(commit_candidates);
    }

    // 3. Symbols in file-mentioned paths
    {
        let symbol_candidates =
            suggest_symbols(store, repo.id, &file_mentions, &terms, limit).await?;
        candidates.extend(symbol_candidates);
    }

    // 4. Routes in file-mentioned paths
    {
        let route_candidates =
            suggest_routes(store, repo.id, &file_mentions, &target_text, limit).await?;
        candidates.extend(route_candidates);
    }

    // Sort by confidence descending
    candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));

    Ok(ProposeLinksResult {
        target_id: target_decision_id,
        target_adr_id,
        target_title,
        candidates,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

async fn resolve_target(
    store: &Arc<SqliteStore>,
    repo_id: uuid::Uuid,
    target: &str,
    warnings: &mut Vec<String>,
) -> Result<(String, String, String, String, Vec<String>, Option<Vec<u8>>), Error> {
    // Try by ADR ID first (e.g. "ADR-0042" or "0042")
    if let Some(row) = store.find_adr_document_by_adr_id(repo_id, target).await? {
        let body = format!(
            "{} {} {}",
            row.context.as_deref().unwrap_or(""),
            row.decision.as_deref().unwrap_or(""),
            row.consequences.as_deref().unwrap_or(""),
        );
        let file_mentions: Vec<String> = row.file_mentions.clone();

        // Load the embedding from the linked decision (if any)
        let (decision_id, embedding) = store
            .find_decision_for_adr(repo_id, row.id)
            .await?
            .map(|(id, emb)| (id, emb))
            .unwrap_or_else(|| (row.id.to_string(), None));

        return Ok((decision_id, row.adr_id, row.title, body, file_mentions, embedding));
    }

    // Try by decision UUID
    if let Ok(uuid) = uuid::Uuid::parse_str(target) {
        if let Some((summary, embedding)) = store.find_decision_with_embedding(repo_id, uuid).await? {
            let file_mentions =
                store.file_mentions_for_decision(repo_id, &summary.id).await?;
            let body = summary.text.clone();
            return Ok((
                summary.id,
                summary.adr_id,
                summary.title,
                body,
                file_mentions,
                embedding,
            ));
        }
    }

    warnings.push(format!(
        "target '{}' did not resolve to a known ADR ID or decision UUID in this repository",
        target
    ));
    Err(Error::InvalidInput {
        field: "target",
        reason: format!("'{}' is not a known ADR ID or decision UUID", target),
    })
}

// ---------------------------------------------------------------------------
// Candidate generators
// ---------------------------------------------------------------------------

async fn suggest_decisions(
    store: &Arc<SqliteStore>,
    repo_id: uuid::Uuid,
    self_id: &str,
    target_text: &str,
    target_embedding: Option<&[u8]>,
    terms: &[String],
    limit: usize,
) -> Result<Vec<LinkCandidate>, Error> {
    use std::collections::HashMap;

    // keyword scores
    let keyword_hits = store
        .search_decisions_ranked(repo_id, target_text, None, 0.0)
        .await?;

    let max_kw = keyword_hits
        .iter()
        .filter(|d| d.id != self_id)
        .map(|d| term_frequency(d, terms))
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut scores: HashMap<String, (String, f64, String)> = HashMap::new(); // id → (title, conf, reason)

    for d in keyword_hits.iter().filter(|d| d.id != self_id) {
        let kw_score = term_frequency(d, terms) as f64 / max_kw;
        if kw_score > 0.0 {
            scores
                .entry(d.id.clone())
                .or_insert_with(|| (d.title.clone(), 0.0, "keyword overlap".to_string()))
                .1 = (scores.get(&d.id).map(|e| e.1).unwrap_or(0.0) + kw_score) / 2.0 + kw_score / 2.0;
        }
    }

    // embedding scores
    if let Some(blob) = target_embedding {
        let query_vec = unpack_f32(blob);
        if !query_vec.is_empty() {
            let pairs = store
                .fetch_decisions_with_embeddings(repo_id, None)
                .await?;
            for (d, emb_blob) in pairs {
                if d.id == self_id {
                    continue;
                }
                if let Some(b) = emb_blob {
                    let vec = unpack_f32(&b);
                    let sim = cosine_similarity(&query_vec, &vec) as f64;
                    if sim >= 0.30 {
                        let entry = scores
                            .entry(d.id.clone())
                            .or_insert_with(|| (d.title.clone(), 0.0, "embedding similarity".to_string()));
                        if sim > entry.1 {
                            entry.1 = (entry.1 + sim) / 2.0;
                            entry.2 = if entry.2.contains("keyword") {
                                "keyword overlap + embedding similarity".to_string()
                            } else {
                                "embedding similarity".to_string()
                            };
                        }
                    }
                }
            }
        }
    }

    let mut out: Vec<LinkCandidate> = scores
        .into_iter()
        .map(|(id, (title, conf, reason))| LinkCandidate {
            entity_type: "decision".to_string(),
            entity_id: id,
            title,
            confidence: (conf * 100.0).round() / 100.0,
            reason,
        })
        .collect();

    out.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit);
    Ok(out)
}

async fn suggest_commits(
    store: &Arc<SqliteStore>,
    repo_id: uuid::Uuid,
    target_text: &str,
    target_embedding: Option<&[u8]>,
    terms: &[String],
    limit: usize,
    warnings: &mut Vec<String>,
) -> Result<Vec<LinkCandidate>, Error> {
    use std::collections::HashMap;

    let mut scores: HashMap<String, (String, String, f64, String)> = HashMap::new(); // id → (sha, msg, conf, reason)

    // keyword pass
    let rows = store.fetch_commits_with_text(repo_id).await?;
    let max_kw = rows
        .iter()
        .map(|(_, _, msg)| term_frequency_str(msg, terms))
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    for (id, sha, msg) in &rows {
        let kw = term_frequency_str(msg, terms) as f64 / max_kw;
        if kw > 0.10 {
            let short_msg = msg.lines().next().unwrap_or("").chars().take(72).collect::<String>();
            scores.entry(id.clone()).or_insert_with(|| {
                (sha.clone(), short_msg, kw * 0.8, "commit message keyword overlap".to_string())
            });
        }
    }

    // embedding pass
    if let Some(blob) = target_embedding {
        let query_vec = unpack_f32(blob);
        if !query_vec.is_empty() {
            let pairs = store.fetch_commits_with_embeddings_all(repo_id).await?;
            if pairs.is_empty() {
                warnings.push(
                    "no commit embeddings found; run embed_all to enable semantic commit matching".to_string(),
                );
            }
            for (id, sha, msg, emb_blob) in pairs {
                if let Some(b) = emb_blob {
                    let vec = unpack_f32(&b);
                    let sim = cosine_similarity(&query_vec, &vec) as f64;
                    if sim >= 0.35 {
                        let short_msg = msg.lines().next().unwrap_or("").chars().take(72).collect::<String>();
                        let entry = scores.entry(id.clone()).or_insert_with(|| {
                            (sha.clone(), short_msg.clone(), 0.0, "embedding similarity".to_string())
                        });
                        if sim > entry.2 {
                            entry.2 = (entry.2 + sim) / 2.0;
                            entry.3 = if entry.3.contains("keyword") {
                                "commit message keyword overlap + embedding similarity".to_string()
                            } else {
                                "embedding similarity".to_string()
                            };
                        }
                    }
                }
            }
        }
    }

    let _ = target_text; // available if needed for future heuristics

    let mut out: Vec<LinkCandidate> = scores
        .into_iter()
        .map(|(id, (sha, msg, conf, reason))| LinkCandidate {
            entity_type: "commit".to_string(),
            entity_id: id,
            title: format!("{} ({})", msg, &sha[..sha.len().min(8)]),
            confidence: (conf * 100.0).round() / 100.0,
            reason,
        })
        .collect();

    out.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit);
    Ok(out)
}

async fn suggest_symbols(
    store: &Arc<SqliteStore>,
    repo_id: uuid::Uuid,
    file_mentions: &[String],
    terms: &[String],
    limit: usize,
) -> Result<Vec<LinkCandidate>, Error> {
    if file_mentions.is_empty() && terms.is_empty() {
        return Ok(vec![]);
    }

    let rows = store
        .fetch_symbols_for_propose(repo_id, file_mentions)
        .await?;

    let mut candidates: Vec<LinkCandidate> = rows
        .into_iter()
        .map(|(id, name, kind, file_path)| {
            // confidence = 0.55 for file-mention match + bonus for name overlap with terms
            let name_lower = name.to_lowercase();
            let name_bonus = if terms.iter().any(|t| name_lower.contains(t.as_str())) {
                0.15
            } else {
                0.0
            };
            let conf = (0.55_f64 + name_bonus).min(1.0);
            let reason = if name_bonus > 0.0 {
                "file mention + symbol name overlap with decision text".to_string()
            } else {
                "symbol in explicitly mentioned file".to_string()
            };
            LinkCandidate {
                entity_type: "symbol".to_string(),
                entity_id: id,
                title: format!("{} {} ({})", kind, name, file_path),
                confidence: (conf * 100.0_f64).round() / 100.0,
                reason,
            }
        })
        .collect();

    candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(limit);
    Ok(candidates)
}

async fn suggest_routes(
    store: &Arc<SqliteStore>,
    repo_id: uuid::Uuid,
    file_mentions: &[String],
    target_text: &str,
    limit: usize,
) -> Result<Vec<LinkCandidate>, Error> {
    if file_mentions.is_empty() {
        return Ok(vec![]);
    }

    let rows = store.fetch_routes_for_propose(repo_id, file_mentions).await?;
    let target_lower = target_text.to_lowercase();

    let mut candidates: Vec<LinkCandidate> = rows
        .into_iter()
        .map(|(id, method, path, file_path)| {
            let route_label = format!("{} {}", method.as_deref().unwrap_or("ANY"), path);
            let path_lower = path.to_lowercase();
            // Check if the route path appears verbatim in the target text
            let text_match = target_lower.contains(&path_lower)
                || path_lower.split('/').filter(|s| s.len() > 3).any(|seg| target_lower.contains(seg));
            let conf = if text_match { 0.72 } else { 0.50 };
            let reason = if text_match {
                "route path referenced in decision text + file mention".to_string()
            } else {
                "route in explicitly mentioned file".to_string()
            };
            LinkCandidate {
                entity_type: "route".to_string(),
                entity_id: id,
                title: format!("{} ({})", route_label, file_path),
                confidence: (conf * 100.0_f64).round() / 100.0,
                reason,
            }
        })
        .collect();

    candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(limit);
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .collect()
}

fn term_frequency(d: &crate::domain::entities::DecisionSummary, terms: &[String]) -> usize {
    let haystack = format!("{} {}", d.title, d.text).to_lowercase();
    terms.iter().map(|t| haystack.matches(t.as_str()).count()).sum()
}

fn term_frequency_str(text: &str, terms: &[String]) -> usize {
    let lower = text.to_lowercase();
    terms.iter().map(|t| lower.matches(t.as_str()).count()).sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_adrs::{run as sync_adrs, SyncAdrsFromGitParams};
    use std::fs;
    use tempfile::tempdir;

    async fn make_store(td: &tempfile::TempDir) -> Arc<SqliteStore> {
        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await.unwrap());
        store.run_migrations().await.unwrap();
        store
    }

    #[tokio::test]
    async fn propose_links_returns_related_decision() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        // Two related ADRs
        let adr1 = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage layer.\n\n## Decision\nUse SQLite for persistence.\n\n## Consequences\nMust not use Postgres.\n";
        let adr2 = "# ADR-0002: SQLite WAL Mode\n\n## Status\nAccepted\n\n## Context\nSQLite concurrency.\n\n## Decision\nEnable WAL mode for SQLite.\n\n## Consequences\nMust configure WAL pragma.\n";
        fs::write(repo.join("0001-sqlite.md"), adr1)?;
        fs::write(repo.join("0002-wal.md"), adr2)?;

        let store = make_store(&td).await;
        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await?;

        let result = run(
            &store,
            ProposeLinksParams {
                repo_path: repo.to_string_lossy().to_string(),
                target: "ADR-0001".to_string(),
                limit: Some(5),
            },
        )
        .await?;

        assert_eq!(result.target_adr_id, "ADR-0001");
        // ADR-0002 mentions "SQLite" — should appear as a candidate decision
        let decision_candidates: Vec<_> = result
            .candidates
            .iter()
            .filter(|c| c.entity_type == "decision")
            .collect();
        assert!(
            !decision_candidates.is_empty(),
            "expected at least one related decision candidate"
        );
        let adr2_candidate = decision_candidates
            .iter()
            .find(|c| c.title.to_lowercase().contains("wal") || c.title.to_lowercase().contains("sqlite"));
        assert!(adr2_candidate.is_some(), "ADR-0002 should be a candidate");
        Ok(())
    }

    #[tokio::test]
    async fn propose_links_unknown_target_returns_error() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git2::Repository::init(&repo).unwrap();

        let store = make_store(&td).await;

        let err = run(
            &store,
            ProposeLinksParams {
                repo_path: repo.to_string_lossy().to_string(),
                target: "ADR-9999".to_string(),
                limit: None,
            },
        )
        .await;

        assert!(err.is_err(), "unknown target should error");
    }
}
