use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::adr_parser;
use crate::domain::entities::AdrStatus;
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateAdrParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Path to the ADR file, absolute or relative to repo_path.
    pub adr_path: String,
}

#[derive(Debug, Serialize)]
pub struct ValidateAdrResult {
    pub valid: bool,
    pub adr_id: Option<String>,
    pub title: String,
    pub status: Option<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: ValidateAdrParams,
) -> Result<ValidateAdrResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    // Resolve adr_path — may be absolute or relative to repo root
    let adr_path = {
        let raw = std::path::Path::new(&params.adr_path);
        if raw.is_absolute() {
            dunce::canonicalize(raw).map_err(|_| Error::InvalidInput {
                field: "adr_path",
                reason: format!("file does not exist: {}", params.adr_path),
            })?
        } else {
            dunce::canonicalize(repo_path.join(raw)).map_err(|_| Error::InvalidInput {
                field: "adr_path",
                reason: format!("file does not exist: {}", params.adr_path),
            })?
        }
    };

    let source = std::fs::read_to_string(&adr_path).map_err(|e| Error::InvalidInput {
        field: "adr_path",
        reason: format!("cannot read file: {e}"),
    })?;

    let stem = adr_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let parsed = adr_parser::parse(&source, stem);

    let mut errors: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];

    // --- Structural checks --------------------------------------------------

    if parsed.title.trim().is_empty() {
        errors.push("missing H1 title".to_string());
    }

    if parsed.adr_id.is_none() {
        errors.push(
            "no ADR ID found — add a numeric ID to the H1 title (e.g. '# ADR-0042: Title') \
             or use a numeric filename stem (e.g. '0042-title.md')"
                .to_string(),
        );
    }

    if parsed.status.is_none() {
        warnings.push("missing Status section".to_string());
    }

    if parsed.context.is_none() {
        warnings.push("missing Context section".to_string());
    }

    if parsed.decision.is_none() {
        warnings.push("missing Decision section".to_string());
    }

    // --- DB consistency checks (only when the ADR has an ID) ----------------

    let repo = store.upsert_repository(repo_path_str, None).await?;

    if let Some(ref adr_id) = parsed.adr_id {
        // Duplicate ID check
        let relative_path = adr_path
            .strip_prefix(&repo_path)
            .ok()
            .and_then(|p| p.to_str())
            .map(|s| s.replace('\\', "/"))
            .unwrap_or_default();

        if let Some(existing) = store.find_adr_by_adr_id_flex(repo.id, adr_id).await? {
            let doc_rel = existing.source_uri.replace('\\', "/");
            if doc_rel != relative_path {
                errors.push(format!(
                    "duplicate ADR ID '{adr_id}' already exists in: {doc_rel}"
                ));
            }
        }

        // Supersession chain checks
        for sup_id in &parsed.supersedes {
            let found = store.find_adr_by_adr_id_flex(repo.id, sup_id).await?;
            if found.is_none() {
                warnings.push(format!(
                    "supersedes '{sup_id}' but that ADR is not in the index — \
                     run sync_adrs_from_git first or check the ID"
                ));
            }
        }

        if let Some(ref sup_by) = parsed.superseded_by {
            let found = store.find_adr_by_adr_id_flex(repo.id, sup_by).await?;
            if found.is_none() {
                warnings.push(format!(
                    "superseded_by '{sup_by}' but that ADR is not in the index"
                ));
            }
        }

        // Invalid status transition: a superseded ADR must reference who superseded it
        if matches!(parsed.status, Some(AdrStatus::Superseded)) && parsed.superseded_by.is_none() {
            warnings.push(
                "status is 'superseded' but no superseded_by reference was found".to_string(),
            );
        }
    }

    let valid = errors.is_empty();
    Ok(ValidateAdrResult {
        valid,
        adr_id: parsed.adr_id,
        title: parsed.title,
        status: parsed.status.map(|s| format!("{s:?}").to_lowercase()),
        errors,
        warnings,
    })
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

    #[tokio::test]
    async fn valid_adr_passes() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        git2::Repository::init(&repo)?;

        let adr_content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nNeed storage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nSimple.\n";
        let adr_file = repo.join("docs/adr/0001-use-sqlite.md");
        fs::write(&adr_file, adr_content)?;

        let store = make_store(&td).await;
        let result = run(
            &store,
            ValidateAdrParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_path: adr_file.to_string_lossy().to_string(),
            },
        )
        .await?;

        assert!(result.valid, "errors: {:?}", result.errors);
        assert_eq!(result.adr_id.as_deref(), Some("ADR-0001"));
        Ok(())
    }

    #[tokio::test]
    async fn missing_adr_id_is_error() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs"))?;
        git2::Repository::init(&repo)?;

        let adr_file = repo.join("docs/README.md");
        fs::write(&adr_file, "# Just a readme\n\n## Status\nAccepted\n")?;

        let store = make_store(&td).await;
        let result = run(
            &store,
            ValidateAdrParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_path: adr_file.to_string_lossy().to_string(),
            },
        )
        .await?;

        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("no ADR ID")));
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_adr_id_flagged() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        git2::Repository::init(&repo)?;

        let content = "# ADR-0001: First\n\n## Status\nAccepted\n\n## Context\nX\n\n## Decision\nY\n";
        let first = repo.join("docs/adr/0001-first.md");
        let second = repo.join("docs/adr/0001-duplicate.md");
        fs::write(&first, content)?;
        fs::write(&second, content)?;

        let store = make_store(&td).await;

        // Ingest the first ADR so the duplicate check has something to compare against
        crate::tools::sync_adrs::run(
            &store,
            crate::tools::sync_adrs::SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/0001-first.md".to_string(),
            },
        )
        .await?;

        let result = run(
            &store,
            ValidateAdrParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_path: second.to_string_lossy().to_string(),
            },
        )
        .await?;

        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("duplicate ADR ID")));
        Ok(())
    }
}
