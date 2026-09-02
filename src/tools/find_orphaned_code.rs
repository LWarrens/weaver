use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindOrphanedCodeParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Optional path prefix filter (relative to repo root, e.g. "src/payments").
    #[serde(default)]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FindOrphanedCodeResult {
    pub orphaned_files: Vec<OrphanedFile>,
    pub orphaned_symbols: Vec<OrphanedSymbol>,
    pub total_files: i64,
    pub total_symbols: i64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OrphanedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct OrphanedSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: Option<i64>,
    pub reason: String,
    /// Compact location anchor: "file:line name"
    pub anchor: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: FindOrphanedCodeParams,
) -> Result<FindOrphanedCodeResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;

    let (orphaned_files, total_files) = store
        .find_orphaned_files(repo.id, params.path_prefix.as_deref())
        .await?;
    let (orphaned_symbols, total_symbols) = store
        .find_orphaned_symbols(repo.id, params.path_prefix.as_deref())
        .await?;

    Ok(FindOrphanedCodeResult {
        orphaned_files: orphaned_files
            .into_iter()
            .map(|path| OrphanedFile {
                path,
                reason: "no linked ADRs or decisions".to_string(),
            })
            .collect(),
        orphaned_symbols: orphaned_symbols
            .into_iter()
            .map(|(name, kind, file, line)| OrphanedSymbol {
                anchor: match line {
                    Some(l) => format!("{}:{} {}", file, l, name),
                    None => format!("{} {}", file, name),
                },
                name,
                kind,
                file,
                line,
                reason: "no governing constraints or decision links".to_string(),
            })
            .collect(),
        total_files,
        total_symbols,
        warnings: vec![],
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ingest_symbols::{run as ingest, IngestSymbolsParams};
    use crate::tools::sync_adrs::{run as sync_adrs, SyncAdrsFromGitParams};
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn orphaned_files_found_when_no_decisions() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("src"))?;
        git2::Repository::init(&repo)?;
        fs::write(repo.join("src/main.rs"), "fn main() {}\n")?;

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        ingest(
            &store,
            IngestSymbolsParams {
                repo_path: repo.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        let result = run(
            &store,
            FindOrphanedCodeParams {
                repo_path: repo.to_string_lossy().to_string(),
                path_prefix: None,
            },
        )
        .await?;

        assert!(result.total_files > 0);
        assert_eq!(result.orphaned_files.len() as i64, result.total_files);
        Ok(())
    }

    #[tokio::test]
    async fn linked_files_not_orphaned() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        fs::create_dir_all(repo.join("src"))?;
        git2::Repository::init(&repo)?;

        // ADR that explicitly mentions src/main.rs
        fs::write(
            repo.join("docs/adr/0001-use-main.md"),
            "# ADR-0001: Use main\n\n## Status\nAccepted\n\n## Context\nSee `src/main.rs`.\n\n## Decision\nUse it.\n",
        )?;
        fs::write(repo.join("src/main.rs"), "fn main() {}\n")?;

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

        ingest(
            &store,
            IngestSymbolsParams {
                repo_path: repo.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        let result = run(
            &store,
            FindOrphanedCodeParams {
                repo_path: repo.to_string_lossy().to_string(),
                path_prefix: None,
            },
        )
        .await?;

        // src/main.rs is mentioned in the ADR, so it should NOT be orphaned
        let orphaned_paths: Vec<&str> =
            result.orphaned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !orphaned_paths.contains(&"src/main.rs"),
            "src/main.rs should not be orphaned: {orphaned_paths:?}"
        );
        Ok(())
    }
}
