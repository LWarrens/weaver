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
pub struct DiffArchitectureParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Start of the diff window — ISO-8601 timestamp or git ref (commit SHA, tag, branch).
    pub from: String,
    /// End of the diff window — ISO-8601 timestamp or git ref. Defaults to now.
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffArchitectureResult {
    pub from_timestamp: String,
    pub to_timestamp: String,
    pub decisions_added: Vec<DiffEntry>,
    pub decisions_removed: Vec<DiffEntry>,
    pub constraints_added: Vec<DiffEntry>,
    pub constraints_removed: Vec<DiffEntry>,
    pub adrs_added: Vec<DiffEntry>,
    pub adrs_removed: Vec<DiffEntry>,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct DiffEntry {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adr_id: Option<String>,
    pub timestamp: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: DiffArchitectureParams,
) -> Result<DiffArchitectureResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let from_ts = resolve_to_timestamp(&params.from, &repo_path).map_err(|e| {
        Error::InvalidInput {
            field: "from",
            reason: e,
        }
    })?;
    let to_ts = match &params.to {
        Some(s) => resolve_to_timestamp(s, &repo_path).map_err(|e| Error::InvalidInput {
            field: "to",
            reason: e,
        })?,
        None => Utc::now().to_rfc3339(),
    };

    let repo = store.upsert_repository(repo_path_str, None).await?;

    let (decisions_added, decisions_removed) = store
        .diff_decisions_in_range(repo.id, &from_ts, &to_ts)
        .await?;
    let (constraints_added, constraints_removed) = store
        .diff_constraints_in_range(repo.id, &from_ts, &to_ts)
        .await?;
    let (adrs_added, adrs_removed) = store
        .diff_adrs_in_range(repo.id, &from_ts, &to_ts)
        .await?;

    let summary = format!(
        "Between {} and {}: {} decision(s) added, {} removed; \
         {} constraint(s) added, {} removed; {} ADR(s) added, {} removed.",
        &from_ts[..10],
        &to_ts[..10],
        decisions_added.len(),
        decisions_removed.len(),
        constraints_added.len(),
        constraints_removed.len(),
        adrs_added.len(),
        adrs_removed.len(),
    );

    Ok(DiffArchitectureResult {
        from_timestamp: from_ts,
        to_timestamp: to_ts,
        decisions_added,
        decisions_removed,
        constraints_added,
        constraints_removed,
        adrs_added,
        adrs_removed,
        summary,
    })
}

/// Resolve a git ref or ISO-8601 string to an RFC-3339 timestamp string.
fn resolve_to_timestamp(input: &str, repo_path: &std::path::Path) -> Result<String, String> {
    // Try ISO-8601 parse first
    if chrono::DateTime::parse_from_rfc3339(input).is_ok() {
        return Ok(input.to_string());
    }
    // Also try date-only (e.g. "2024-01-15")
    if let Ok(date) = chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(chrono::DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc).to_rfc3339());
    }

    // Try as a git ref
    let git_repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("cannot open git repository: {e}"))?;
    let obj = git_repo
        .revparse_single(input)
        .map_err(|_| format!("'{input}' is not a valid ISO-8601 timestamp or git ref"))?;
    let commit = obj
        .peel_to_commit()
        .map_err(|_| format!("git ref '{input}' does not point to a commit"))?;
    let epoch = commit.time().seconds();
    let dt = chrono::DateTime::from_timestamp(epoch, 0)
        .ok_or_else(|| format!("git commit timestamp out of range for ref '{input}'"))?;
    Ok(dt.to_rfc3339())
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
    async fn diff_shows_newly_added_decision() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let store = make_store(&td).await;

        let before = Utc::now().to_rfc3339();

        // Ingest an ADR
        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nSimple.\n";
        fs::write(repo.join("0001-sqlite.md"), content)?;
        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await?;

        let after = Utc::now().to_rfc3339();

        let result = run(
            &store,
            DiffArchitectureParams {
                repo_path: repo.to_string_lossy().to_string(),
                from: before,
                to: Some(after),
            },
        )
        .await?;

        assert!(
            !result.decisions_added.is_empty(),
            "expected at least one decision added; got {:?}",
            result.decisions_added
        );
        assert!(result.adrs_added.iter().any(|a| a.adr_id.as_deref() == Some("ADR-0001")));
        Ok(())
    }

    #[tokio::test]
    async fn diff_empty_range_has_no_changes() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let store = make_store(&td).await;

        let t0 = "2020-01-01T00:00:00Z";
        let t1 = "2020-01-02T00:00:00Z";

        let result = run(
            &store,
            DiffArchitectureParams {
                repo_path: repo.to_string_lossy().to_string(),
                from: t0.to_string(),
                to: Some(t1.to_string()),
            },
        )
        .await?;

        assert!(result.decisions_added.is_empty());
        assert!(result.decisions_removed.is_empty());
        assert!(result.adrs_added.is_empty());
        Ok(())
    }
}
