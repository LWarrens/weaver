use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RetractParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Type of entity to retract: "decision", "constraint", or "episode".
    /// Retracting an episode closes all decisions and constraints derived from it.
    pub entity_type: String,
    /// UUID of the entity to retract.
    pub entity_id: String,
    /// Human-readable reason for the retraction (stored in the audit trail).
    pub reason: String,
    /// Optional correction text. If provided, a retraction-correction episode is
    /// recorded as an audit note (no LLM extraction is triggered).
    #[serde(default)]
    pub replacement: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RetractResult {
    /// Whether at least one active (valid_to IS NULL) entity was closed.
    pub retracted: bool,
    pub entity_type: String,
    pub entity_id: String,
    pub retracted_at: String,
    pub reason: String,
    /// Number of decisions closed (1 for decision, 0 for constraint, N for episode).
    pub decisions_closed: i64,
    /// Number of constraints closed (cascaded from closed decisions).
    pub constraints_closed: i64,
    /// ID of the correction episode, if `replacement` was provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_episode_id: Option<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: RetractParams,
) -> Result<RetractResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let entity_id = uuid::Uuid::parse_str(&params.entity_id).map_err(|_| Error::InvalidInput {
        field: "entity_id",
        reason: format!("not a valid UUID: {}", params.entity_id),
    })?;

    let entity_type = params.entity_type.as_str();
    match entity_type {
        "decision" | "constraint" | "episode" => {}
        other => {
            return Err(Error::InvalidInput {
                field: "entity_type",
                reason: format!(
                    "unknown entity type '{other}'; must be 'decision', 'constraint', or 'episode'"
                ),
            });
        }
    }

    let now = Utc::now().to_rfc3339();
    let mut warnings: Vec<String> = vec![];

    let (decisions_closed, constraints_closed) = match entity_type {
        "decision" => {
            let (d, c) = store.retract_decision(entity_id, &now).await?;
            if d == 0 {
                warnings.push(format!(
                    "decision '{}' was not found or was already retracted",
                    params.entity_id
                ));
            }
            (d, c)
        }
        "constraint" => {
            let c = store.retract_constraint(entity_id, &now).await?;
            if c == 0 {
                warnings.push(format!(
                    "constraint '{}' was not found or was already retracted",
                    params.entity_id
                ));
            }
            (0, c)
        }
        "episode" => {
            // Verify the episode exists in this repo before acting
            let _ = store.upsert_repository(repo_path_str, None).await?;
            let (d, c) = store.retract_episode_facts(entity_id, &now).await?;
            if d == 0 && c == 0 {
                warnings.push(format!(
                    "episode '{}' had no active decisions or constraints to retract",
                    params.entity_id
                ));
            }
            (d, c)
        }
        _ => unreachable!(),
    };

    let retracted = decisions_closed > 0 || constraints_closed > 0;

    // --- Optional correction note -------------------------------------------
    let replacement_episode_id = if let Some(ref replacement_text) = params.replacement {
        let episode_id = uuid::Uuid::new_v4();
        let content = format!(
            "Retraction correction for {} '{}': {}\n\nReason for retraction: {}",
            entity_type, params.entity_id, replacement_text, params.reason
        );
        store
            .insert_retraction_episode(
                episode_id,
                &content,
                &params.entity_id,
                entity_type,
                &now,
            )
            .await?;
        Some(episode_id.to_string())
    } else {
        None
    };

    Ok(RetractResult {
        retracted,
        entity_type: entity_type.to_string(),
        entity_id: params.entity_id,
        retracted_at: now,
        reason: params.reason,
        decisions_closed,
        constraints_closed,
        replacement_episode_id,
        warnings,
    })
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
    async fn retract_decision_closes_it_and_constraints() -> Result<(), Box<dyn std::error::Error>>
    {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nMust not use Postgres.\n";
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

        // Find the decision ID
        let decisions = store
            .search_decisions(
                store
                    .upsert_repository(
                        repo.canonicalize()?.to_str().unwrap(),
                        None,
                    )
                    .await?
                    .id,
                "SQLite",
                None,
            )
            .await?;
        assert!(!decisions.is_empty(), "expected at least one decision");
        let decision_id = decisions[0].id.clone();

        let result = run(
            &store,
            RetractParams {
                repo_path: repo.to_string_lossy().to_string(),
                entity_type: "decision".to_string(),
                entity_id: decision_id.clone(),
                reason: "test retraction".to_string(),
                replacement: None,
            },
        )
        .await?;

        assert!(result.retracted, "should have retracted");
        assert_eq!(result.decisions_closed, 1);
        assert!(result.constraints_closed >= 1, "should have closed constraints");

        // Running again should find nothing active
        let result2 = run(
            &store,
            RetractParams {
                repo_path: repo.to_string_lossy().to_string(),
                entity_type: "decision".to_string(),
                entity_id: decision_id,
                reason: "second attempt".to_string(),
                replacement: None,
            },
        )
        .await?;

        assert!(!result2.retracted, "already retracted — should be idempotent");
        assert!(!result2.warnings.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn retract_with_replacement_records_episode() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let content = "# ADR-0001: Use Redis\n\n## Status\nAccepted\n\n## Context\nCaching.\n\n## Decision\nUse Redis.\n\n## Consequences\nFast.\n";
        fs::write(repo.join("0001-redis.md"), content)?;

        let store = make_store(&td).await;
        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await?;

        let repo_row = store
            .upsert_repository(repo.canonicalize()?.to_str().unwrap(), None)
            .await?;
        let decisions = store
            .search_decisions(repo_row.id, "Redis", None)
            .await?;
        let decision_id = decisions[0].id.clone();

        let result = run(
            &store,
            RetractParams {
                repo_path: repo.to_string_lossy().to_string(),
                entity_type: "decision".to_string(),
                entity_id: decision_id,
                reason: "LLM hallucinated Redis; actual decision was Memcached".to_string(),
                replacement: Some("Use Memcached for caching instead".to_string()),
            },
        )
        .await?;

        assert!(result.retracted);
        assert!(
            result.replacement_episode_id.is_some(),
            "correction episode should have been recorded"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_entity_type_returns_error() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await.unwrap());
        store.run_migrations().await.unwrap();

        let err = run(
            &store,
            RetractParams {
                repo_path: repo.to_string_lossy().to_string(),
                entity_type: "adr".to_string(),
                entity_id: uuid::Uuid::new_v4().to_string(),
                reason: "test".to_string(),
                replacement: None,
            },
        )
        .await;

        assert!(err.is_err());
    }
}
