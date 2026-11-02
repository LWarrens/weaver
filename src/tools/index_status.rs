use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::sqlite::IndexStatusCounts;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexStatusParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
}

#[derive(Debug, Serialize)]
pub struct IndexStatusResult {
    pub repo_path: String,
    pub index_state: IndexState,
    /// Per-lane freshness: last-indexed commit, commit lag vs HEAD, status, and
    /// the query capabilities each lane currently enables.
    pub lanes: Vec<crate::domain::entities::LaneStatus>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexState {
    pub adrs: EntityStatus,
    pub decisions: EmbeddableStatus,
    pub constraints: EmbeddableStatus,
    pub episodes: EmbeddableStatus,
    pub commits: EmbeddableStatus,
    pub pull_requests: EmbeddableStatus,
    pub symbols: EmbeddableStatus,
    pub files: SimpleStatus,
}

#[derive(Debug, Serialize)]
pub struct EntityStatus {
    pub total: i64,
    pub last_sync_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddableStatus {
    pub total: i64,
    pub embedded: i64,
    pub coverage: f64,
    pub last_ingested_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SimpleStatus {
    pub total: i64,
    pub last_ingested_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: IndexStatusParams,
) -> Result<IndexStatusResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;
    let c: IndexStatusCounts = store.index_status_counts(repo.id).await?;
    let lanes = crate::tools::freshness::lane_statuses(store, repo.id, &repo_path)
        .await
        .unwrap_or_default();

    // No git HEAD means no commit lag; fall back to comparing timestamps.
    let warnings = crate::tools::freshness::timestamp_staleness_warnings(store, repo.id, &repo_path)
        .await
        .unwrap_or_default();

    fn coverage(embedded: i64, total: i64) -> f64 {
        if total == 0 { 1.0 } else { embedded as f64 / total as f64 }
    }

    Ok(IndexStatusResult {
        repo_path: repo_path_str.to_string(),
        index_state: IndexState {
            adrs: EntityStatus {
                total: c.adrs_total,
                last_sync_at: c.adrs_last_ingested_at,
            },
            decisions: EmbeddableStatus {
                total: c.decisions_total,
                embedded: c.decisions_embedded,
                coverage: coverage(c.decisions_embedded, c.decisions_total),
                last_ingested_at: c.decisions_last_ingested_at,
            },
            constraints: EmbeddableStatus {
                total: c.constraints_total,
                embedded: c.constraints_embedded,
                coverage: coverage(c.constraints_embedded, c.constraints_total),
                last_ingested_at: c.constraints_last_ingested_at,
            },
            episodes: EmbeddableStatus {
                total: c.episodes_total,
                embedded: c.episodes_embedded,
                coverage: coverage(c.episodes_embedded, c.episodes_total),
                last_ingested_at: c.episodes_last_ingested_at,
            },
            commits: EmbeddableStatus {
                total: c.commits_total,
                embedded: c.commits_embedded,
                coverage: coverage(c.commits_embedded, c.commits_total),
                last_ingested_at: c.commits_last_ingested_at,
            },
            pull_requests: EmbeddableStatus {
                total: c.prs_total,
                embedded: c.prs_embedded,
                coverage: coverage(c.prs_embedded, c.prs_total),
                last_ingested_at: c.prs_last_ingested_at,
            },
            symbols: EmbeddableStatus {
                total: c.symbols_total,
                embedded: c.symbols_embedded,
                coverage: coverage(c.symbols_embedded, c.symbols_total),
                last_ingested_at: c.symbols_last_ingested_at,
            },
            files: SimpleStatus {
                total: c.files_total,
                last_ingested_at: c.files_last_ingested_at,
            },
        },
        lanes,
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

    // --- timestamp staleness fallback (no git HEAD) -------------------------

    async fn store_in(dir: &std::path::Path) -> Result<Arc<SqliteStore>, Box<dyn std::error::Error>> {
        let db_url = format!("sqlite:{}?mode=rwc", dir.join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;
        Ok(store)
    }

    async fn index(store: &Arc<SqliteStore>, repo: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        crate::tools::ingest_symbols::run(
            store,
            crate::tools::ingest_symbols::IngestSymbolsParams {
                repo_path: repo.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;
        Ok(())
    }

    async fn status(store: &Arc<SqliteStore>, repo: &std::path::Path) -> Result<IndexStatusResult, Box<dyn std::error::Error>> {
        Ok(run(store, IndexStatusParams { repo_path: repo.to_string_lossy().to_string() }).await?)
    }

    /// Give `path` a modification time an hour in the future, so it is newer than
    /// any lane that was indexed a moment ago.
    fn touch_future(path: &std::path::Path) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
    }

    #[tokio::test]
    async fn non_git_repo_warns_when_source_changed_after_indexing() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("src"))?;
        fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n")?;
        let store = store_in(td.path()).await?;
        index(&store, &repo).await?;

        let fresh = status(&store, &repo).await?;
        assert!(fresh.warnings.is_empty(), "nothing changed yet: {:?}", fresh.warnings);
        let symbol = fresh.lanes.iter().find(|l| l.lane == "symbol").unwrap();
        assert_eq!(symbol.lag_commits, None, "no git, so no commit lag");

        touch_future(&repo.join("src/lib.rs"));
        let stale = status(&store, &repo).await?;
        assert!(
            stale.warnings.iter().any(|w| w.contains("lane 'symbol' may be stale") && w.contains("no git HEAD")),
            "warnings: {:?}",
            stale.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_git_staleness_ignores_files_ingest_would_skip() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("src"))?;
        fs::create_dir_all(repo.join("node_modules/dep"))?;
        fs::create_dir_all(repo.join("target"))?;
        fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n")?;
        let store = store_in(td.path()).await?;
        index(&store, &repo).await?;

        // An image, a dependency, and a build artifact changing must not read as stale.
        for f in ["src/logo.png", "node_modules/dep/index.js", "target/out.rs"] {
            fs::write(repo.join(f), "x")?;
            touch_future(&repo.join(f));
        }
        let result = status(&store, &repo).await?;
        assert!(result.warnings.is_empty(), "warnings: {:?}", result.warnings);
        Ok(())
    }

    #[tokio::test]
    async fn adr_lane_staleness_only_considers_documentation() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        fs::create_dir_all(repo.join("src"))?;
        fs::write(
            repo.join("docs/adr/0001-x.md"),
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse the bus.\n",
        )?;
        fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n")?;
        let store = store_in(td.path()).await?;
        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await?;
        index(&store, &repo).await?;

        touch_future(&repo.join("src/lib.rs"));
        let result = status(&store, &repo).await?;
        assert!(result.warnings.iter().any(|w| w.contains("lane 'symbol'")), "{:?}", result.warnings);
        assert!(
            !result.warnings.iter().any(|w| w.contains("lane 'adr'")),
            "a source edit must not make the ADR lane stale: {:?}",
            result.warnings
        );
        Ok(())
    }

    /// With a git HEAD, lag is measured in commits instead; no timestamp warning.
    #[tokio::test]
    async fn repo_with_a_head_reports_no_timestamp_warning() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("src"))?;
        fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n")?;
        let git = git2::Repository::init(&repo)?;
        let mut idx = git.index()?;
        idx.add_path(std::path::Path::new("src/lib.rs"))?;
        idx.write()?;
        let tree = git.find_tree(idx.write_tree()?)?;
        let sig = git2::Signature::now("t", "t@t")?;
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])?;

        let store = store_in(td.path()).await?;
        index(&store, &repo).await?;
        touch_future(&repo.join("src/lib.rs"));

        let result = status(&store, &repo).await?;
        assert!(result.warnings.is_empty(), "warnings: {:?}", result.warnings);
        let symbol = result.lanes.iter().find(|l| l.lane == "symbol").unwrap();
        assert_eq!(symbol.lag_commits, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn index_status_empty_repo() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        // init bare git repo so dunce::canonicalize works
        git2::Repository::init(&repo)?;

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        let result = run(
            &store,
            IndexStatusParams {
                repo_path: repo.to_string_lossy().to_string(),
            },
        )
        .await?;

        assert_eq!(result.index_state.adrs.total, 0);
        assert_eq!(result.index_state.decisions.total, 0);
        assert_eq!(result.index_state.symbols.total, 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_status_after_adr_sync() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        git2::Repository::init(&repo)?;

        fs::write(
            repo.join("docs/adr/0001-use-sqlite.md"),
            "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nNeed a DB.\n\n## Decision\nUse SQLite.\n",
        )?;

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await?;

        let result = run(
            &store,
            IndexStatusParams {
                repo_path: repo.to_string_lossy().to_string(),
            },
        )
        .await?;

        assert_eq!(result.index_state.adrs.total, 1);
        assert!(result.index_state.decisions.total > 0);
        assert_eq!(result.index_state.decisions.embedded, 0);
        assert_eq!(result.index_state.decisions.coverage, 0.0);

        // sync_adrs records the `adr` lane with its capabilities.
        let adr_lane = result
            .lanes
            .iter()
            .find(|l| l.lane == "adr")
            .expect("adr lane recorded");
        assert_eq!(adr_lane.status, "ok");
        assert!(!adr_lane.capabilities.is_empty());
        Ok(())
    }
}
