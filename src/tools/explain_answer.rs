use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::embeddings::provider_from_env;
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainAnswerParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// The same query you passed to query — re-runs it with full provenance.
    pub query: String,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExplainAnswerResult {
    pub query: String,
    pub terms_extracted: Vec<String>,
    pub steps: Vec<ExplanationStep>,
    pub final_decision_ids: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ExplanationStep {
    /// "keyword_search" | "semantic_search" | "graph_expansion" | "rrf_merge"
    pub step: String,
    pub description: String,
    pub matched: Vec<MatchedEntry>,
}

#[derive(Debug, Serialize)]
pub struct MatchedEntry {
    pub decision_id: String,
    pub title: String,
    pub adr_id: String,
    pub score: f64,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: ExplainAnswerParams,
) -> Result<ExplainAnswerResult, Error> {
    let now = Utc::now().to_rfc3339();

    if params.query.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "query",
            reason: "query must not be empty".to_string(),
        });
    }

    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {ts}"),
        })?;
    }

    let valid_at = params.valid_at.as_deref();
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let mut steps: Vec<ExplanationStep> = vec![];
    let mut warnings: Vec<String> = vec![];

    // --- Step 1: keyword / FTS search ---------------------------------------
    let keyword_decisions = store
        .search_decisions_ranked(repo.id, &params.query, valid_at, 0.0)
        .await?;

    let terms_extracted: Vec<String> = params
        .query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .collect();

    let keyword_matched: Vec<MatchedEntry> = keyword_decisions
        .iter()
        .enumerate()
        .map(|(rank, d)| {
            let tf = term_frequency_score(d, &terms_extracted);
            MatchedEntry {
                decision_id: d.id.clone(),
                title: d.title.clone(),
                adr_id: d.adr_id.clone(),
                score: tf as f64,
                reason: format!(
                    "matched {} of {} terms at keyword rank {}",
                    tf,
                    terms_extracted.len(),
                    rank + 1
                ),
            }
        })
        .collect();

    steps.push(ExplanationStep {
        step: "keyword_search".to_string(),
        description: format!(
            "FTS over decisions and constraints for terms: {}",
            terms_extracted.join(", ")
        ),
        matched: keyword_matched,
    });

    // --- Step 2: semantic / vector search -----------------------------------
    let semantic_decisions = if let Some(provider) = provider_from_env() {
        match provider.embed_chunked(&params.query, 512).await {
            Ok(embedding) => {
                let results = store
                    .semantic_decisions_if_available(repo.id, Some(&embedding), valid_at, 0.0)
                    .await?;
                if let Some(ref list) = results {
                    let matched: Vec<MatchedEntry> = list
                        .iter()
                        .enumerate()
                        .map(|(rank, d)| MatchedEntry {
                            decision_id: d.id.clone(),
                            title: d.title.clone(),
                            adr_id: d.adr_id.clone(),
                            score: d.confidence,
                            reason: format!("cosine similarity rank {}", rank + 1),
                        })
                        .collect();
                    steps.push(ExplanationStep {
                        step: "semantic_search".to_string(),
                        description: "Vector cosine similarity against stored decision embeddings."
                            .to_string(),
                        matched,
                    });
                }
                results
            }
            Err(e) => {
                warnings.push(format!("Semantic search failed: {e}"));
                None
            }
        }
    } else {
        warnings.push(
            "Semantic retrieval skipped: no embedding provider configured (WEAVER_EMBEDDING_PROVIDER)."
                .to_string(),
        );
        None
    };

    // --- Step 3: graph expansion --------------------------------------------
    let seed_ids = keyword_decisions
        .iter()
        .chain(semantic_decisions.as_deref().unwrap_or(&[]).iter())
        .take(5)
        .map(|d| d.id.clone())
        .collect::<Vec<_>>();

    let graph_decisions = store
        .graph_neighbor_decisions(repo.id, &seed_ids, 1, valid_at, 0.0)
        .await?;

    let seed_set: std::collections::HashSet<&str> = seed_ids.iter().map(|s| s.as_str()).collect();
    let graph_matched: Vec<MatchedEntry> = graph_decisions
        .iter()
        .filter(|d| !seed_set.contains(d.id.as_str()))
        .map(|d| MatchedEntry {
            decision_id: d.id.clone(),
            title: d.title.clone(),
            adr_id: d.adr_id.clone(),
            score: d.confidence,
            reason: format!(
                "reachable via temporal edge from seed decisions: {}",
                seed_ids.join(", ")
            ),
        })
        .collect();

    steps.push(ExplanationStep {
        step: "graph_expansion".to_string(),
        description: format!(
            "BFS over temporal_edges from {} seed decision(s), depth 1.",
            seed_ids.len()
        ),
        matched: graph_matched,
    });

    // --- Step 4: RRF merge --------------------------------------------------
    const RRF_K: f64 = 60.0;
    let mut scores = std::collections::HashMap::<String, f64>::new();
    let mut id_to_entry = std::collections::HashMap::<String, MatchedEntry>::new();

    for list in [
        &keyword_decisions[..],
        semantic_decisions.as_deref().unwrap_or(&[]),
        &graph_decisions[..],
    ] {
        for (rank, d) in list.iter().enumerate() {
            let rrf = 1.0 / (RRF_K + rank as f64 + 1.0);
            *scores.entry(d.id.clone()).or_insert(0.0) += rrf;
            id_to_entry.entry(d.id.clone()).or_insert_with(|| MatchedEntry {
                decision_id: d.id.clone(),
                title: d.title.clone(),
                adr_id: d.adr_id.clone(),
                score: 0.0,
                reason: String::new(),
            });
        }
    }
    let mut merged: Vec<MatchedEntry> = id_to_entry
        .into_values()
        .map(|mut e| {
            let rrf = *scores.get(&e.decision_id).unwrap_or(&0.0);
            e.score = rrf;
            e.reason = format!("RRF score {rrf:.4}");
            e
        })
        .collect();
    merged.sort_by(|a, b| b.score.total_cmp(&a.score));
    merged.truncate(10);

    let final_decision_ids: Vec<String> = merged.iter().map(|e| e.decision_id.clone()).collect();

    steps.push(ExplanationStep {
        step: "rrf_merge".to_string(),
        description: format!(
            "Reciprocal rank fusion (k={RRF_K}) across keyword, semantic, and graph retrievers. \
             Top {} decision(s) selected.",
            final_decision_ids.len()
        ),
        matched: merged,
    });

    let _ = now; // suppress unused warning when no semantic provider

    Ok(ExplainAnswerResult {
        query: params.query,
        terms_extracted,
        steps,
        final_decision_ids,
        warnings,
    })
}

fn term_frequency_score(
    d: &crate::domain::entities::DecisionSummary,
    terms: &[String],
) -> usize {
    let text = format!("{} {}", d.title, d.text).to_lowercase();
    terms.iter().filter(|t| text.contains(t.as_str())).count()
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
    async fn explain_shows_keyword_step() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite for persistence.\n\n## Consequences\nSimple.\n";
        fs::write(repo.join("0001-sqlite.md"), content)?;

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
            ExplainAnswerParams {
                repo_path: repo.to_string_lossy().to_string(),
                query: "sqlite persistence".to_string(),
                valid_at: None,
            },
        )
        .await?;

        assert!(result.terms_extracted.contains(&"sqlite".to_string()));
        assert!(result.steps.iter().any(|s| s.step == "keyword_search"));
        assert!(result.steps.iter().any(|s| s.step == "rrf_merge"));
        assert!(!result.final_decision_ids.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn explain_empty_query_returns_error() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await.unwrap());
        store.run_migrations().await.unwrap();

        let err = run(
            &store,
            ExplainAnswerParams {
                repo_path: repo.to_string_lossy().to_string(),
                query: "  ".to_string(),
                valid_at: None,
            },
        )
        .await;

        assert!(err.is_err());
    }
}
