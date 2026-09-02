use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::entities::TemporalContext;
use crate::error::Error;
use crate::storage::SqliteStore;

const MAX_COMMUNITIES: usize = 30;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetArchitectureParams {
    pub repo_path: String,
    #[serde(default)]
    pub valid_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommunitySummary {
    pub label: String,
    pub size: usize,
    pub central_symbols: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DecisionRef {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct GetArchitectureResult {
    pub answer: String,
    pub repo_path: String,
    pub counts: BTreeMap<String, i64>,
    pub active_decisions: Vec<DecisionRef>,
    pub communities: Vec<CommunitySummary>,
    pub total_communities: usize,
    pub warnings: Vec<String>,
    pub temporal_context: TemporalContext,
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: GetArchitectureParams,
) -> Result<GetArchitectureResult, Error> {
    let now = Utc::now().to_rfc3339();
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!(
                "path does not exist or is not accessible: {}",
                params.repo_path
            ),
        })?;

    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    let repo = store.upsert_repository(repo_path_str, None).await?;
    let valid_at = params.valid_at.as_deref();
    let all_decisions = store.list_all_decisions(repo.id, valid_at).await?;
    let active_decisions: Vec<DecisionRef> = all_decisions
        .into_iter()
        .map(|d| DecisionRef {
            id: d.id,
            title: d.title,
            status: d.status,
        })
        .collect();

    let counts = store.repository_counts(repo.id).await?;

    let raw_communities = store.get_communities_for_repo(repo.id).await?;
    let total_communities = raw_communities.len();
    let communities: Vec<CommunitySummary> = raw_communities
        .into_iter()
        .take(MAX_COMMUNITIES)
        .map(|(_comm_id, label, size, _file_paths, sym_names)| CommunitySummary {
            label,
            size,
            central_symbols: sym_names.into_iter().take(5).collect(),
        })
        .collect();

    let mut warnings = vec![];
    if active_decisions.is_empty() {
        warnings.push(
            "no active decisions found; run sync_adrs_from_git or record_decision_episode".to_string(),
        );
    }
    if counts.get("symbols").copied().unwrap_or_default() == 0 {
        warnings.push(
            "no symbols indexed; run ingest_symbols for code-level lookup".to_string(),
        );
    }
    if counts.get("temporal_edges").copied().unwrap_or_default() == 0 {
        warnings.push("general temporal graph edges are not populated yet".to_string());
    }

    Ok(GetArchitectureResult {
        answer: format!(
            "Repository has {} active decision(s), {} constraint(s), {} indexed symbol(s), and {} detected communit(y/ies){}.",
            active_decisions.len(),
            counts.get("constraints").copied().unwrap_or_default(),
            counts.get("symbols").copied().unwrap_or_default(),
            total_communities,
            if total_communities > MAX_COMMUNITIES {
                format!(" (showing top {})", MAX_COMMUNITIES)
            } else {
                String::new()
            },
        ),
        repo_path: repo_path_str.to_string(),
        counts,
        active_decisions,
        communities,
        total_communities,
        warnings,
        temporal_context: TemporalContext {
            valid_at: params.valid_at,
            ingested_at: Some(now),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_empty_architecture_with_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");
        let store = Arc::new(store);

        let result = run(
            &store,
            GetArchitectureParams {
                repo_path: dir.path().to_string_lossy().to_string(),
                valid_at: None,
            },
        )
        .await
        .expect("architecture");

        assert!(result.answer.contains("0 active decision"));
        assert!(!result.warnings.is_empty());
    }
}
