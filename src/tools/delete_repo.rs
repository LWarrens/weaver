use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteRepoParams {
    /// Absolute path to the repository root (must match a stored repository path exactly).
    pub repo_path: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteRepoResult {
    pub deleted: bool,
    pub repo_path: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: DeleteRepoParams,
) -> Result<DeleteRepoResult, Error> {
    let repo_path = params.repo_path.trim().to_string();

    let deleted = store.delete_repository(&repo_path).await?;

    if deleted {
        Ok(DeleteRepoResult {
            deleted: true,
            repo_path,
            message: "Repository and all associated data deleted.".to_string(),
        })
    } else {
        Ok(DeleteRepoResult {
            deleted: false,
            repo_path,
            message: "Repository not found; nothing was deleted.".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const ADR: &str = r#"# ADR-0001: Use a response cache

## Status

Accepted

## Context

Latency is too high.

## Decision

We will use a response cache in `src/cache.rs`. All reads must go through it.

## Consequences

More memory use.
"#;

    /// Exercise every ingestion path, then assert `delete_repository` leaves
    /// nothing behind anywhere in the schema.
    #[tokio::test]
    async fn delete_repository_empties_every_table() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        // Mock LLM so episode fact extraction produces free-text temporal edges,
        // which are the ones that cannot be scoped by entity id.
        std::env::set_var("WEAVER_LLM_PROVIDER", "mock");
        std::env::set_var(
            "WEAVER_LLM_RESPONSE",
            r#"[{"subject":"api","relation":"must_use","object":"cache","confidence":0.8}]"#,
        );

        let td = tempfile::tempdir()?;
        let root = td.path().join("repo");
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("0001-cache.md"), ADR)?;
        fs::write(
            root.join("src/cache.rs"),
            "pub fn get() -> u32 { 1 }\npub fn put(v: u32) -> u32 { get() + v }\n",
        )?;

        let git_repo = git2::Repository::init(&root)?;
        let sig = git2::Signature::now("Test", "test@test.invalid")?;
        let commit_all = |msg: &str| {
            let mut index = git_repo.index().unwrap();
            index
                .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git_repo.find_tree(tree_id).unwrap();
            let parent = git_repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.as_ref().map(|c| vec![c]).unwrap_or_default();
            git_repo
                .commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
                .unwrap()
        };
        commit_all("Initial import");
        // A squash-merge PR commit citing the ADR, so PR rows and pr-typed links exist.
        commit_all("Add the response cache (#4)\n\nImplements ADR-0001.");

        let repo_path = root.to_str().unwrap().to_string();
        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        crate::tools::sync_adrs::run(
            &store,
            crate::tools::sync_adrs::SyncAdrsFromGitParams {
                repo_path: repo_path.clone(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await?;
        crate::tools::ingest_symbols::run(
            &store,
            crate::tools::ingest_symbols::IngestSymbolsParams {
                repo_path: repo_path.clone(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;
        crate::tools::sync_commits::run(
            &store,
            crate::tools::sync_commits::SyncCommitsFromGitParams {
                repo_path: repo_path.clone(),
                branch: None,
                since: None,
                limit: 500,
            },
        )
        .await?;
        crate::tools::sync_prs::run(
            &store,
            crate::tools::sync_prs::SyncPrsFromGitParams {
                repo_path: repo_path.clone(),
                branch: None,
                since: None,
                limit: 200,
                enrich: Some(false),
                mine_decisions: Some(true),
            },
        )
        .await?;
        // An episode-sourced decision: the branch delete_repository used to miss.
        crate::tools::record_episode::run(
            &store,
            crate::tools::record_episode::RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "meeting:design".to_string(),
                source_uri: None,
                occurred_at: "2026-01-02T03:04:05Z".to_string(),
                content: "We agreed the api must use the cache.".to_string(),
                decisions: Some(vec![crate::tools::record_episode::EpisodeDecision {
                    title: Some("Api uses the cache".to_string()),
                    text: "The api layer routes reads through the cache.".to_string(),
                    constraints: vec!["The api must use the cache.".to_string()],
                    affected_files: vec!["src/cache.rs".to_string()],
                    entities: vec!["api".to_string()],
                }]),
                dedup_threshold: None,
            },
        )
        .await?;

        // `table_count` is allowlisted, so count directly for full coverage.
        async fn count(store: &SqliteStore, table: &str) -> Result<i64, Error> {
            Ok(
                sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{table}\""))
                    .fetch_one(store.pool())
                    .await?,
            )
        }

        // Sanity: the pipeline actually populated the tables we are about to check.
        for table in [
            "repositories", "adr_documents", "decisions", "constraints", "files", "symbols",
            "commits", "pull_requests", "episodes", "decision_git_links", "temporal_edges",
            "claims", "evidence_anchors", "index_lanes",
        ] {
            assert!(
                count(&store, table).await? > 0,
                "precondition: `{table}` should be populated before deletion"
            );
        }

        let result = run(&store, DeleteRepoParams { repo_path: repo_path.clone() }).await?;
        assert!(result.deleted, "repository should have been deleted");

        // Every table in the schema must now be empty. Enumerated from
        // sqlite_master rather than hardcoded, so a table added later without a
        // matching cascade fails this test instead of silently orphaning rows.
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations'",
        )
        .fetch_all(store.pool())
        .await?;
        assert!(tables.len() > 15, "expected the full schema, got {tables:?}");

        for table in tables {
            let remaining = count(&store, &table).await?;
            assert_eq!(
                remaining, 0,
                "`{table}` still holds {remaining} rows after delete_repository"
            );
        }

        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_LLM_RESPONSE");
        Ok(())
    }
}
