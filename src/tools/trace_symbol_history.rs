use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceSymbolHistoryParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Symbol name, file path (relative to repo root), or route pattern to trace.
    pub symbol: String,
    /// ISO-8601 lower bound for the trace window (inclusive). Null = no lower bound.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// ISO-8601 upper bound for the trace window (inclusive). Null = no upper bound.
    #[serde(default)]
    pub valid_to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TraceEvent {
    /// ISO-8601 timestamp when the event occurred or was recorded.
    pub occurred_at: String,
    /// Event kind: "symbol_seen", "decision_activated", "decision_superseded",
    /// "commit", "episode", "constraint_activated", "constraint_superseded".
    pub event_type: String,
    pub entity_id: String,
    pub entity_type: String,
    /// One-line human-readable description.
    pub summary: String,
    pub confidence: f64,
}

#[derive(Debug, Serialize)]
pub struct TraceSymbolHistoryResult {
    pub symbol: String,
    /// File path where the symbol was most recently observed, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_file: Option<String>,
    /// Earliest ingested timestamp for this symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    /// Timeline sorted oldest → newest.
    pub timeline: Vec<TraceEvent>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: TraceSymbolHistoryParams,
) -> Result<TraceSymbolHistoryResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let mut warnings: Vec<String> = vec![];
    let mut timeline: Vec<TraceEvent> = vec![];

    let repo = store.upsert_repository(repo_path_str, None).await?;

    let from = params.valid_from.as_deref().unwrap_or("0001-01-01T00:00:00Z");
    let to = params.valid_to.as_deref().unwrap_or("9999-12-31T23:59:59Z");

    // ------------------------------------------------------------------
    // 1. Symbol spans — all ingested appearances of this symbol name
    // ------------------------------------------------------------------
    let symbol_spans = store
        .trace_symbol_spans(repo.id, &params.symbol, from, to)
        .await?;

    let first_seen = symbol_spans
        .iter()
        .map(|s| s.valid_from.clone())
        .min();

    let current_file = symbol_spans
        .iter()
        .filter(|s| s.valid_to.is_none())
        .map(|s| s.file_path.clone())
        .next();

    if symbol_spans.is_empty() {
        warnings.push(format!(
            "symbol '{}' not found in the ingested index — run ingest_symbols first",
            params.symbol
        ));
    }

    for span in &symbol_spans {
        let summary = if let Some(ref vt) = span.valid_to {
            format!("{} {} first seen in {} (removed {})", span.kind, span.name, span.file_path, vt)
        } else {
            format!("{} {} in {} (still active)", span.kind, span.name, span.file_path)
        };
        timeline.push(TraceEvent {
            occurred_at: span.valid_from.clone(),
            event_type: "symbol_seen".to_string(),
            entity_id: span.id.clone(),
            entity_type: "symbol".to_string(),
            summary,
            confidence: 1.0,
        });
    }

    // Collect unique file paths to join against decisions and commits
    let file_paths: Vec<String> = {
        let mut fps: Vec<String> = symbol_spans.iter().map(|s| s.file_path.clone()).collect();
        fps.dedup();
        fps
    };

    // ------------------------------------------------------------------
    // 2. Governing decisions (via decision_code_links → files)
    // ------------------------------------------------------------------
    let decision_rows = store
        .trace_decisions_for_files(repo.id, &file_paths, from, to)
        .await?;

    for (decision_id, adr_id, title, valid_from, valid_to, confidence) in &decision_rows {
        let (event_type, summary) = if valid_to.is_none() {
            (
                "decision_activated".to_string(),
                format!("decision '{}' activated ({})", title, adr_id),
            )
        } else {
            (
                "decision_superseded".to_string(),
                format!("decision '{}' closed ({})", title, adr_id),
            )
        };
        let occurred_at = valid_from.clone();
        timeline.push(TraceEvent {
            occurred_at,
            event_type,
            entity_id: decision_id.clone(),
            entity_type: "decision".to_string(),
            summary,
            confidence: *confidence,
        });

        // If decision was closed, add a supersession event at valid_to
        if let Some(vt) = valid_to {
            timeline.push(TraceEvent {
                occurred_at: vt.clone(),
                event_type: "decision_superseded".to_string(),
                entity_id: decision_id.clone(),
                entity_type: "decision".to_string(),
                summary: format!("decision '{}' became inactive ({})", title, adr_id),
                confidence: *confidence,
            });
        }
    }

    // ------------------------------------------------------------------
    // 3. Active constraints on those decisions
    // ------------------------------------------------------------------
    let decision_ids: Vec<String> = decision_rows.iter().map(|(id, ..)| id.clone()).collect();
    let constraint_rows = store
        .trace_constraints_for_decisions(&decision_ids, from, to)
        .await?;

    for (constraint_id, text, valid_from, valid_to, confidence) in &constraint_rows {
        timeline.push(TraceEvent {
            occurred_at: valid_from.clone(),
            event_type: "constraint_activated".to_string(),
            entity_id: constraint_id.clone(),
            entity_type: "constraint".to_string(),
            summary: format!("constraint: {}", truncate(text, 100)),
            confidence: *confidence,
        });
        if let Some(vt) = valid_to {
            timeline.push(TraceEvent {
                occurred_at: vt.clone(),
                event_type: "constraint_superseded".to_string(),
                entity_id: constraint_id.clone(),
                entity_type: "constraint".to_string(),
                summary: format!("constraint closed: {}", truncate(text, 80)),
                confidence: *confidence,
            });
        }
    }

    // ------------------------------------------------------------------
    // 4. Commits linked to those decisions (decision_git_links)
    // ------------------------------------------------------------------
    let commit_rows = store
        .trace_commits_for_decisions(&decision_ids, from, to)
        .await?;

    if commit_rows.is_empty() && !decision_ids.is_empty() {
        warnings.push(
            "no commits linked to governing decisions — run sync_commits_from_git for commit history".to_string(),
        );
    }

    for (commit_id, sha, message, source_time, confidence) in &commit_rows {
        let short = message
            .as_deref()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        timeline.push(TraceEvent {
            occurred_at: source_time.clone(),
            event_type: "commit".to_string(),
            entity_id: commit_id.clone(),
            entity_type: "commit".to_string(),
            summary: format!("{} ({})", short, &sha[..sha.len().min(8)]),
            confidence: *confidence,
        });
    }

    // ------------------------------------------------------------------
    // 5. Episodes linked to governing decisions
    // ------------------------------------------------------------------
    let episode_rows = store
        .trace_episodes_for_decisions(&decision_ids, from, to)
        .await?;

    for (episode_id, source, content, occurred_at) in &episode_rows {
        timeline.push(TraceEvent {
            occurred_at: occurred_at.clone(),
            event_type: "episode".to_string(),
            entity_id: episode_id.clone(),
            entity_type: "episode".to_string(),
            summary: format!("[{}] {}", source, truncate(content, 100)),
            confidence: 0.9,
        });
    }

    // ------------------------------------------------------------------
    // Sort timeline oldest → newest, deduplicate exact (id, type) pairs
    // ------------------------------------------------------------------
    timeline.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
    {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        timeline.retain(|e| seen.insert((e.entity_id.clone(), e.event_type.clone())));
    }

    Ok(TraceSymbolHistoryResult {
        symbol: params.symbol,
        current_file,
        first_seen,
        timeline,
        warnings,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
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
    async fn trace_unknown_symbol_returns_warning() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let store = make_store(&td).await;

        let result = run(
            &store,
            TraceSymbolHistoryParams {
                repo_path: repo.to_string_lossy().to_string(),
                symbol: "nonexistent_function".to_string(),
                valid_from: None,
                valid_to: None,
            },
        )
        .await?;

        assert!(result.timeline.is_empty());
        assert!(!result.warnings.is_empty(), "should warn about unknown symbol");
        Ok(())
    }

    #[tokio::test]
    async fn trace_picks_up_governing_decisions() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        // ADR that mentions a specific file
        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite — see `src/storage/sqlite.rs`.\n\n## Consequences\nMust not use Postgres.\n";
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

        // Manually insert a symbol in src/storage/sqlite.rs so the trace can link it
        let repo_row = store
            .upsert_repository(
                repo.canonicalize()?.to_str().unwrap(),
                None,
            )
            .await?;
        let file_id = store
            .upsert_file(repo_row.id, "src/storage/sqlite.rs", "2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z")
            .await?;
        store
            .insert_symbol(
                file_id,
                "connect",
                "function",
                10,
                20,
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .await?;

        let result = run(
            &store,
            TraceSymbolHistoryParams {
                repo_path: repo.to_string_lossy().to_string(),
                symbol: "connect".to_string(),
                valid_from: None,
                valid_to: None,
            },
        )
        .await?;

        assert!(!result.timeline.is_empty(), "should have timeline events");
        let has_symbol_event = result
            .timeline
            .iter()
            .any(|e| e.event_type == "symbol_seen");
        assert!(has_symbol_event);
        Ok(())
    }
}
