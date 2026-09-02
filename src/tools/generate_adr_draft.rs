use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateAdrDraftParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Title for the new ADR, e.g. "Use Postgres for user data".
    pub title: String,
    /// Background and motivation for this decision.
    #[serde(default)]
    pub context: Option<String>,
    /// The decision being proposed.
    #[serde(default)]
    pub proposed_decision: Option<String>,
    /// File or module paths affected by this decision.
    #[serde(default)]
    pub affected_files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GenerateAdrDraftResult {
    /// The next ADR identifier that would be assigned, e.g. "ADR-0003".
    pub id: String,
    /// Rendered Markdown content ready to be written to a file.
    pub markdown: String,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: GenerateAdrDraftParams,
) -> Result<GenerateAdrDraftResult, Error> {
    // --- Validation ---------------------------------------------------------
    if params.title.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "title",
            reason: "title must not be empty".to_string(),
        });
    }

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

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // --- Derive next ADR number ---------------------------------------------
    let n = store.next_adr_number(repo.id).await?;
    let id = format!("ADR-{:04}", n);

    // --- Find related decisions from existing ADRs --------------------------
    let warnings: Vec<String> = vec![];
    let related = store.search_decisions(repo.id, &params.title, None).await?;
    let related_ids: Vec<String> = related.into_iter().take(5).map(|d| d.adr_id).collect();

    // --- Render Markdown template -------------------------------------------
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let markdown = render_template(RenderParams {
        id: &id,
        title: &params.title,
        date: &today,
        context: params.context.as_deref(),
        proposed_decision: params.proposed_decision.as_deref(),
        affected_files: &params.affected_files,
        related_ids: &related_ids,
    });

    Ok(GenerateAdrDraftResult {
        id,
        markdown,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

struct RenderParams<'a> {
    id: &'a str,
    title: &'a str,
    date: &'a str,
    context: Option<&'a str>,
    proposed_decision: Option<&'a str>,
    affected_files: &'a [String],
    related_ids: &'a [String],
}

fn render_template(p: RenderParams<'_>) -> String {
    let mut out = String::new();

    out.push_str(&format!("# {}: {}\n\n", p.id, p.title));

    out.push_str("## Status\n\nProposed\n\n");

    out.push_str(&format!("Date: {}\n\n", p.date));

    out.push_str("## Context\n\n");
    match p.context {
        Some(c) if !c.trim().is_empty() => {
            out.push_str(c.trim());
            out.push_str("\n\n");
        }
        _ => out.push_str("_Not specified_\n\n"),
    }

    out.push_str("## Decision\n\n");
    match p.proposed_decision {
        Some(d) if !d.trim().is_empty() => {
            out.push_str(d.trim());
            out.push_str("\n\n");
        }
        _ => out.push_str("_Not specified_\n\n"),
    }

    out.push_str("## Consequences\n\n_Not specified_\n\n");

    out.push_str("## Constraints\n\n_None specified_\n\n");

    out.push_str("## Alternatives Considered\n\n_None_\n\n");

    out.push_str("## Affected Code\n\n");
    if p.affected_files.is_empty() {
        out.push_str("_None identified_\n\n");
    } else {
        for f in p.affected_files {
            out.push_str(&format!("- `{}`\n", f));
        }
        out.push('\n');
    }

    out.push_str("## Related Decisions\n\n");
    if p.related_ids.is_empty() {
        out.push_str("_None_\n");
    } else {
        for id in p.related_ids {
            out.push_str(&format!("- {}\n", id));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_adrs::{run as sync_adrs, SyncAdrsFromGitParams};

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    const ADR_1: &str = r#"# ADR-0001 Use SQLite

## Status

Accepted. Date: 2024-01-01.

## Context

Need embedded db.

## Decision

Use SQLite.

## Consequences

- Lightweight.
"#;

    const ADR_2: &str = r#"# ADR-0002 Use Redis for cache

## Status

Accepted. Date: 2024-02-01.

## Context

Need a cache layer.

## Decision

Use Redis.

## Consequences

- Fast.
"#;

    #[tokio::test]
    async fn generates_next_id_and_all_sections() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001.md"), ADR_1).expect("write");
        std::fs::write(dir.path().join("0002.md"), ADR_2).expect("write");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let result = run(
            &store,
            GenerateAdrDraftParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                title: "Switch to Postgres".to_string(),
                context: Some("SQLite limits write throughput.".to_string()),
                proposed_decision: Some("Use Postgres 16 with connection pooling.".to_string()),
                affected_files: vec!["src/storage/db.rs".to_string()],
            },
        )
        .await
        .expect("generate");

        assert_eq!(result.id, "ADR-0003");
        assert!(result.markdown.contains("# ADR-0003: Switch to Postgres"));
        assert!(result.markdown.contains("## Status"));
        assert!(result.markdown.contains("## Context"));
        assert!(result.markdown.contains("## Decision"));
        assert!(result.markdown.contains("## Consequences"));
        assert!(result.markdown.contains("## Constraints"));
        assert!(result.markdown.contains("## Alternatives Considered"));
        assert!(result.markdown.contains("## Affected Code"));
        assert!(result.markdown.contains("## Related Decisions"));
        assert!(result.markdown.contains("src/storage/db.rs"));
        assert!(
            result.warnings.is_empty(),
            "unexpected warnings: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn first_adr_gets_id_0001_when_store_empty() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        let result = run(
            &store,
            GenerateAdrDraftParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                title: "Initial architecture decision".to_string(),
                context: None,
                proposed_decision: None,
                affected_files: vec![],
            },
        )
        .await
        .expect("generate");

        assert_eq!(result.id, "ADR-0001");
    }
}
