use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::sync_adrs::{run as sync_adrs, SyncAdrsFromGitParams};
use crate::tools::ingest_symbols::{run as ingest_symbols, IngestSymbolsParams};

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncIncrementalParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Only re-index files changed after this point — ISO-8601 timestamp or git ref
    /// (commit SHA, tag, branch name). Defaults to the most recent ingestion timestamp
    /// in the index; if the index is empty, indexes everything.
    #[serde(default)]
    pub since: Option<String>,
    /// Glob pattern used to match ADR files (relative to repo root).
    /// Defaults to "docs/adr/*.md".
    #[serde(default)]
    pub adr_glob: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SyncIncrementalResult {
    /// The resolved "since" timestamp used for the diff.
    pub since_resolved: String,
    /// Files identified as changed in the window.
    pub changed_files: Vec<String>,
    /// ADR files that were re-synced.
    pub adrs_resynced: Vec<String>,
    /// Source files whose symbols were re-ingested.
    pub sources_reindexed: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: SyncIncrementalParams,
) -> Result<SyncIncrementalResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let adr_glob = params
        .adr_glob
        .as_deref()
        .unwrap_or("docs/adr/*.md")
        .to_string();

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // --- Resolve "since" ----------------------------------------------------
    let mut warnings: Vec<String> = vec![];
    // Raw `since` string — may be ISO timestamp or git ref
    let since_input: String = match &params.since {
        Some(s) => s.clone(),
        None => {
            // Use the most recent ingestion timestamp across all entity types
            let counts = store.index_status_counts(repo.id).await?;
            let candidates = [
                &counts.adrs_last_ingested_at,
                &counts.decisions_last_ingested_at,
                &counts.symbols_last_ingested_at,
                &counts.commits_last_ingested_at,
            ];
            candidates
                .iter()
                .filter_map(|x| x.as_deref())
                .max()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
        }
    };

    // --- Find changed files via git -----------------------------------------
    // git_available = false means the walk itself failed (no git repo, corrupt, etc.).
    let (changed_files, since_display, git_available) =
        match find_changed_since(&repo_path, &since_input) {
            Ok((files, display)) => (files, display, true),
            Err(e) => {
                warnings.push(format!(
                    "git diff unavailable ({e}); falling back to full index"
                ));
                (vec![], since_input.clone(), false)
            }
        };

    // Separate ADR candidates from source candidates.
    let adr_exts = ["md"];
    let source_exts = ["rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "cs", "cpp", "c", "h"];

    let (adr_files, source_files): (Vec<_>, Vec<_>) = changed_files.iter().partition(|p| {
        let ext = std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        adr_exts.contains(&ext)
    });

    let source_files: Vec<_> = source_files
        .into_iter()
        .filter(|p| {
            let ext = std::path::Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            source_exts.contains(&ext)
        })
        .collect();

    // --- Re-sync ADRs -------------------------------------------------------
    // Only sync if git failed entirely (full fallback) or ADR files actually changed.
    let mut adrs_resynced: Vec<String> = vec![];
    if !git_available || !adr_files.is_empty() {
        sync_adrs(
            store,
            SyncAdrsFromGitParams {
                repo_path: repo_path_str.to_string(),
                adr_glob: adr_glob.clone(),
            },
        )
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("ADR sync failed: {e}")))?;
        adrs_resynced = if !git_available {
            vec!["<all ADRs>".to_string()]
        } else {
            adr_files.into_iter().cloned().collect()
        };
    }

    // --- Re-ingest symbols for changed source files -------------------------
    let mut sources_reindexed: Vec<String> = vec![];
    if !source_files.is_empty() {
        // Build a glob that matches each changed source file exactly
        for src in &source_files {
            ingest_symbols(
                store,
                IngestSymbolsParams {
                    repo_path: repo_path_str.to_string(),
                    pattern: Some(src.to_string()),
                    force: false,
                },
                None,
                None,
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("symbol ingest failed for {}: {}", src, e)))?;
        }
        sources_reindexed = source_files.into_iter().cloned().collect();
    }

    Ok(SyncIncrementalResult {
        since_resolved: since_display,
        changed_files,
        adrs_resynced,
        sources_reindexed,
        warnings,
    })
}

/// Find files changed since `since_input` (ISO-8601 timestamp or git ref).
/// Returns (changed_files, display_string).
///
/// When `since_input` is a git ref, uses `revwalk.hide()` for exact exclusion —
/// this avoids false negatives from git's second-precision timestamps.
/// When it is a timestamp, falls back to timestamp comparison (exclusive).
fn find_changed_since(
    repo_path: &std::path::Path,
    since_input: &str,
) -> Result<(Vec<String>, String), String> {
    let git_repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("cannot open git repository: {e}"))?;

    let mut revwalk = git_repo
        .revwalk()
        .map_err(|e| format!("revwalk failed: {e}"))?;
    revwalk
        .push_head()
        .map_err(|e| format!("cannot push HEAD to revwalk: {e}"))?;

    // Determine how to bound the walk.
    let since_epoch: Option<i64>;
    let display: String;

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(since_input) {
        // ISO-8601 path: use timestamp comparison
        since_epoch = Some(dt.timestamp());
        display = since_input.to_string();
    } else if let Ok(obj) = git_repo.revparse_single(since_input) {
        // Git ref path: use revwalk.hide() for exact exclusion
        if let Ok(commit) = obj.peel_to_commit() {
            revwalk
                .hide(commit.id())
                .map_err(|e| format!("revwalk hide failed: {e}"))?;
            let epoch = commit.time().seconds();
            let dt = chrono::DateTime::from_timestamp(epoch, 0)
                .ok_or_else(|| "commit timestamp out of range".to_string())?;
            display = dt.to_rfc3339();
            since_epoch = None; // hide() already handles the boundary
        } else {
            return Err(format!(
                "git ref '{since_input}' does not point to a commit"
            ));
        }
    } else {
        return Err(format!(
            "'{since_input}' is not a valid ISO-8601 timestamp or git ref"
        ));
    }

    let mut changed = std::collections::HashSet::new();

    for oid in revwalk.flatten() {
        let commit = match git_repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // When using timestamp mode, stop before the boundary (exclusive).
        if let Some(epoch) = since_epoch {
            if commit.time().seconds() < epoch {
                break;
            }
        }
        collect_commit_files(&git_repo, &commit, &mut changed);
    }

    let mut result: Vec<String> = changed.into_iter().collect();
    result.sort();
    Ok((result, display))
}

fn collect_commit_files(
    git_repo: &git2::Repository,
    commit: &git2::Commit,
    changed: &mut std::collections::HashSet<String>,
) {
    if commit.parent_count() == 0 {
        if let Ok(tree) = commit.tree() {
            if let Ok(diff) = git_repo.diff_tree_to_tree(None, Some(&tree), None) {
                diff.deltas().for_each(|d| {
                    if let Some(path) = d.new_file().path().and_then(|p| p.to_str()) {
                        changed.insert(path.replace('\\', "/"));
                    }
                });
            }
        }
    } else {
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let child_tree = commit.tree().ok();
        if let (Some(pt), Some(ct)) = (parent_tree, child_tree) {
            if let Ok(diff) = git_repo.diff_tree_to_tree(Some(&pt), Some(&ct), None) {
                diff.deltas().for_each(|d| {
                    if let Some(path) = d.new_file().path().and_then(|p| p.to_str()) {
                        changed.insert(path.replace('\\', "/"));
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    async fn make_store(td: &tempfile::TempDir) -> Arc<SqliteStore> {
        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = Arc::new(SqliteStore::connect(&db_url).await.unwrap());
        store.run_migrations().await.unwrap();
        store
    }

    fn git_commit_all(repo: &git2::Repository, message: &str) {
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().map(|c| vec![c]).unwrap_or_default();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents).unwrap();
    }

    #[tokio::test]
    async fn incremental_syncs_adrs_from_scratch() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo_dir = td.path().join("repo");
        fs::create_dir_all(repo_dir.join("docs/adr"))?;
        let git_repo = git2::Repository::init(&repo_dir)?;

        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nSimple.\n";
        fs::write(repo_dir.join("docs/adr/0001-use-sqlite.md"), content)?;
        git_commit_all(&git_repo, "add ADR");

        let store = make_store(&td).await;
        let result = run(
            &store,
            SyncIncrementalParams {
                repo_path: repo_dir.to_string_lossy().to_string(),
                since: None,
                adr_glob: Some("docs/adr/*.md".to_string()),
            },
        )
        .await?;

        assert!(!result.adrs_resynced.is_empty(), "should have synced ADRs");
        Ok(())
    }

    #[tokio::test]
    async fn incremental_with_since_limits_scope() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo_dir = td.path().join("repo");
        fs::create_dir_all(repo_dir.join("docs/adr"))?;
        let git_repo = git2::Repository::init(&repo_dir)?;

        // First commit — an ADR
        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nSimple.\n";
        fs::write(repo_dir.join("docs/adr/0001-use-sqlite.md"), content)?;
        git_commit_all(&git_repo, "add ADR");

        // Capture the first commit SHA to use as the `since` git ref
        let first_sha = git_repo
            .head()?
            .peel_to_commit()?
            .id()
            .to_string();

        // Second commit — a source file only
        fs::create_dir_all(repo_dir.join("src"))?;
        fs::write(repo_dir.join("src/main.rs"), "fn main() {}")?;
        git_commit_all(&git_repo, "add source");

        let store = make_store(&td).await;
        let result = run(
            &store,
            SyncIncrementalParams {
                repo_path: repo_dir.to_string_lossy().to_string(),
                // Use the git ref (first commit SHA) — avoids second-precision issues
                since: Some(first_sha),
                adr_glob: Some("docs/adr/*.md".to_string()),
            },
        )
        .await?;

        // Only src/main.rs changed after the first commit — no ADRs in the diff
        assert!(
            result.adrs_resynced.is_empty(),
            "no ADRs should be synced when only src changed: {:?}",
            result.adrs_resynced
        );
        assert!(
            result.changed_files.iter().any(|f| f.contains("main.rs")),
            "src/main.rs should appear in changed files: {:?}",
            result.changed_files
        );
        Ok(())
    }
}
