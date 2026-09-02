use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::domain::entities::{ArchResponse, TemporalContext, TemporalMode};
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDecisionsForCodeParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// The code entity to resolve. Provide at least one field.
    pub target: Target,
    /// ISO-8601 timestamp to query as-of. Defaults to current time if omitted.
    #[serde(default)]
    pub valid_at: Option<String>,
    /// Which timeline valid_at applies to. Defaults to event time.
    #[serde(default)]
    pub temporal_mode: TemporalMode,
    /// Return full decision and constraint text. Defaults to false: text is
    /// truncated to a 280-char snippet. Set true when you need the full ADR body.
    #[serde(default)]
    pub include_full_text: bool,
    /// Freshness verification mode for the evidence behind the returned
    /// decisions: `cached` (default), `fresh`, `skip`, or `strict`.
    #[serde(default)]
    pub verify: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Target {
    /// File path relative to the repository root (Phase 1: only this is resolved).
    #[serde(default)]
    pub file: Option<String>,
    /// Symbol name (Phase 2: tree-sitter required).
    #[serde(default)]
    pub symbol: Option<String>,
    /// Module name (Phase 2).
    #[serde(default)]
    pub module: Option<String>,
    /// Service name (Phase 2).
    #[serde(default)]
    pub service: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: FindDecisionsForCodeParams,
) -> Result<ArchResponse, Error> {
    let now = Utc::now().to_rfc3339();

    // --- Input validation ---------------------------------------------------
    if params.target.file.is_none()
        && params.target.symbol.is_none()
        && params.target.module.is_none()
        && params.target.service.is_none()
    {
        return Err(Error::InvalidInput {
            field: "target",
            reason: "at least one of file, symbol, module, or service must be provided".to_string(),
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

    // Validate valid_at is parseable ISO-8601 if provided
    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    // --- Resolve repository -------------------------------------------------
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let valid_at = params.valid_at.as_deref();
    let mut response = ArchResponse::empty();
    response.temporal_context = TemporalContext {
        valid_at: params.valid_at.clone(),
        ingested_at: Some(now.clone()),
    };
    let mut warnings: Vec<String> = vec![];

    if let Some(w) = crate::tools::freshness::stale_index_warning(&store, repo.id, &repo_path).await {
        warnings.push(w);
    }

    // --- Phase 1: file-level lookup ----------------------------------------
    if let Some(ref file) = params.target.file {
        // Validate path safety without requiring the file to exist on disk.
        // Decisions can reference files that have since been renamed or deleted.
        let joined = repo_path.join(file);
        let normalized = normalize_lexical(&joined);

        if !normalized.starts_with(&repo_path) {
            return Err(Error::InvalidInput {
                field: "target.file",
                reason: "file path escapes the repository root".to_string(),
            });
        }

        let rel = normalized
            .strip_prefix(&repo_path)
            .expect("just checked starts_with")
            .to_string_lossy()
            .replace('\\', "/");

        let decisions = store
            .find_decisions_for_file(repo.id, &rel, valid_at, params.temporal_mode)
            .await?;

        if decisions.is_empty() {
            warnings.push(format!(
                "no decisions linked to file '{}' (file may not have been mentioned in any ADR, or ADRs have not been synced)",
                rel
            ));
        }

        let decision_ids: Vec<String> = decisions.iter().map(|d| d.id.clone()).collect();
        let constraints = store.find_constraints_for_decisions(&decision_ids).await?;

        // Build a summary answer
        if !decisions.is_empty() {
            response.answer = Some(format!(
                "Found {} decision(s) linked to '{}'.",
                decisions.len(),
                rel
            ));
        }

        // Confidence is the minimum across all returned decisions
        response.confidence = decisions
            .iter()
            .map(|d| d.confidence)
            .fold(f64::INFINITY, f64::min);
        if response.confidence == f64::INFINITY {
            response.confidence = 0.0;
        }

        response.decisions = decisions;
        response.constraints = constraints;

        // Append route info if this file has routes
        let file_routes = store.find_routes_for_repo(repo.id).await.unwrap_or_default();
        let file_rel_routes: Vec<_> = file_routes
            .into_iter()
            .filter(|(fp, _, _, _)| fp == &rel)
            .collect();
        if !file_rel_routes.is_empty() {
            let route_strs: Vec<String> = file_rel_routes
                .iter()
                .map(|(_, method, path, _)| {
                    format!("{} {}", method.as_deref().unwrap_or("*"), path)
                })
                .collect();
            let route_note = format!(
                "File contains {} route(s): {}.",
                file_rel_routes.len(),
                route_strs.join(", ")
            );
            response.answer = Some(match response.answer.take() {
                Some(prev) => format!("{} {}", prev, route_note),
                None => route_note,
            });
        }
    }

    // --- Phase 2: symbol-level lookup ----------------------------------------
    if let Some(ref symbol) = params.target.symbol {
        let files = store
            .find_files_with_symbol(repo.id, symbol, valid_at)
            .await?;

        if files.is_empty() {
            warnings.push(format!(
                "symbol '{}' not found in symbol index — run ingest_symbols first",
                symbol
            ));
        } else {
            let mut seen: HashSet<String> =
                response.decisions.iter().map(|d| d.id.clone()).collect();

            for file_path in &files {
                let file_decisions = store
                    .find_decisions_for_file(repo.id, file_path, valid_at, params.temporal_mode)
                    .await?;

                let new_decisions: Vec<_> = file_decisions
                    .into_iter()
                    .filter(|d| seen.insert(d.id.clone()))
                    .collect();

                if !new_decisions.is_empty() {
                    let constraint_ids: Vec<String> =
                        new_decisions.iter().map(|d| d.id.clone()).collect();
                    let new_constraints = store
                        .find_constraints_for_decisions(&constraint_ids)
                        .await?;
                    response.constraints.extend(new_constraints);
                    response.decisions.extend(new_decisions);
                }
            }

            if !response.decisions.is_empty() {
                let answer = format!(
                    "Found {} decision(s) linked to symbol '{}' (via {} file(s)).",
                    response.decisions.len(),
                    symbol,
                    files.len()
                );
                response.answer = Some(match response.answer.take() {
                    Some(prev) => format!("{} {}", prev, answer),
                    None => answer,
                });
                response.confidence = response
                    .decisions
                    .iter()
                    .map(|d| d.confidence)
                    .fold(f64::INFINITY, f64::min);
                if response.confidence == f64::INFINITY {
                    response.confidence = 0.0;
                }
            } else {
                warnings.push(format!(
                    "symbol '{}' found in {} file(s) but no decisions are linked to those files",
                    symbol,
                    files.len()
                ));
            }
        }
    }

    // --- Phase 3: module/service entity resolution ---------------------------
    // Modules and services resolve through `entity_nodes` (populated by ADR
    // service/module mentions and episode entities) via open `mentions`
    // Decision → Entity edges. Modules additionally fall back to path-segment
    // matching over `decision_code_links` (module "storage" matches decisions
    // linked to files under any `storage/` directory).
    for (kind, name) in [
        ("module", &params.target.module),
        ("service", &params.target.service),
    ] {
        let Some(name) = name else { continue };

        let entities = store
            .find_entity_nodes_by_name(repo.id, name, Some(kind))
            .await?;
        let entity_ids: Vec<String> = entities.iter().map(|e| e.id.to_string()).collect();
        let mut decision_ids = store
            .decision_ids_mentioning_entities(&entity_ids, valid_at)
            .await?;
        let via_entity = decision_ids.len();

        if kind == "module" {
            decision_ids.extend(store.decision_ids_linked_under_path_segment(name).await?);
        }
        decision_ids.sort_unstable();
        decision_ids.dedup();

        if decision_ids.is_empty() {
            warnings.push(format!(
                "{} '{}' did not resolve to any entity or linked decision — \
                 sync ADRs or record episodes mentioning it first",
                kind, name
            ));
            continue;
        }

        let found = store
            .find_decisions_by_ids(repo.id, &decision_ids, valid_at, 0.0)
            .await?;
        let mut seen: HashSet<String> = response.decisions.iter().map(|d| d.id.clone()).collect();
        let new_decisions: Vec<_> = found
            .into_iter()
            .filter(|d| seen.insert(d.id.clone()))
            .collect();

        if !new_decisions.is_empty() {
            let ids: Vec<String> = new_decisions.iter().map(|d| d.id.clone()).collect();
            response
                .constraints
                .extend(store.find_constraints_for_decisions(&ids).await?);
            let note = format!(
                "Found {} decision(s) linked to {} '{}'{}.",
                new_decisions.len(),
                kind,
                name,
                if via_entity == 0 {
                    " (via file-path match only)"
                } else {
                    ""
                }
            );
            response.decisions.extend(new_decisions);
            response.answer = Some(match response.answer.take() {
                Some(prev) => format!("{} {}", prev, note),
                None => note,
            });
        }
    }

    // Recompute confidence as the minimum across all returned decisions.
    if !response.decisions.is_empty() {
        response.confidence = response
            .decisions
            .iter()
            .map(|d| d.confidence)
            .fold(f64::INFINITY, f64::min);
    }

    response.warnings = warnings;

    if !params.include_full_text {
        crate::tools::architecture_query::truncate_response_text(&mut response);
    }

    let view_key = format!(
        "{}|{}|{}|{}|{}",
        params.target.file.as_deref().unwrap_or(""),
        params.target.symbol.as_deref().unwrap_or(""),
        params.target.module.as_deref().unwrap_or(""),
        params.target.service.as_deref().unwrap_or(""),
        params.valid_at.as_deref().unwrap_or(""),
    );
    crate::tools::freshness::attach_manifest(
        &store,
        repo.id,
        &repo_path,
        "find_decisions_for_code",
        &view_key,
        &mut response,
        crate::tools::freshness::VerifyMode::parse(params.verify.as_deref()),
    )
    .await?;

    Ok(response)
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve `.` and `..` components lexically without touching the filesystem.
/// This lets us validate path safety for files that may not currently exist.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = vec![];
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last normal component; if nothing to pop, keep the `..`
                // so that the result escapes the expected root and gets caught.
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStore;
    use crate::tools::record_episode::{self, EpisodeDecision, RecordDecisionEpisodeParams};
    use crate::tools::sync_adrs::{self, SyncAdrsFromGitParams};

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    fn params(repo_path: &str, target: Target) -> FindDecisionsForCodeParams {
        FindDecisionsForCodeParams {
            repo_path: repo_path.to_string(),
            target,
            valid_at: None,
            temporal_mode: Default::default(),
            include_full_text: false,
                verify: None,
        }
    }

    #[tokio::test]
    async fn resolves_service_via_entity_mentions() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        record_episode::run(
            &store,
            RecordDecisionEpisodeParams {
                repo_path: repo_path.clone(),
                source: "meeting:billing".to_string(),
                source_uri: None,
                occurred_at: "2026-07-01T09:00:00Z".to_string(),
                content: "Billing rework discussion.".to_string(),
                decisions: Some(vec![EpisodeDecision {
                    title: Some("Async invoicing".to_string()),
                    text: "Invoicing becomes asynchronous behind a queue.".to_string(),
                    constraints: vec![],
                    affected_files: vec![],
                    entities: vec!["billing".to_string()],
                }]),
                dedup_threshold: None,
            },
        )
        .await
        .expect("episode");

        let resp = run(
            &store,
            params(
                &repo_path,
                Target {
                    file: None,
                    symbol: None,
                    module: None,
                    service: Some("Billing".to_string()),
                },
            ),
        )
        .await
        .expect("find");

        assert_eq!(resp.decisions.len(), 1, "warnings: {:?}", resp.warnings);
        assert_eq!(resp.decisions[0].title, "Async invoicing");
        assert!(resp
            .answer
            .as_deref()
            .unwrap()
            .contains("service 'Billing'"));
    }

    #[tokio::test]
    async fn resolves_module_via_path_segment_fallback() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        std::fs::write(
            dir.path().join("0001-sqlite.md"),
            "# ADR-0001: Use SQLite\n\n## Status\n\nAccepted\n\n## Decision\n\nWe will use SQLite in `src/storage/db.rs`.\n",
        )
        .unwrap();
        sync_adrs::run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo_path.clone(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let resp = run(
            &store,
            params(
                &repo_path,
                Target {
                    file: None,
                    symbol: None,
                    module: Some("storage".to_string()),
                    service: None,
                },
            ),
        )
        .await
        .expect("find");

        assert_eq!(resp.decisions.len(), 1, "warnings: {:?}", resp.warnings);
        assert_eq!(resp.decisions[0].adr_id, "ADR-0001");
        assert!(
            resp.answer
                .as_deref()
                .unwrap()
                .contains("via file-path match only"),
            "answer: {:?}",
            resp.answer
        );
    }

    #[tokio::test]
    async fn unresolved_module_warns() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        let resp = run(
            &store,
            params(
                &repo_path,
                Target {
                    file: None,
                    symbol: None,
                    module: Some("nonexistent".to_string()),
                    service: None,
                },
            ),
        )
        .await
        .expect("find");

        assert!(resp.decisions.is_empty());
        assert!(
            resp.warnings
                .iter()
                .any(|w| w.contains("module 'nonexistent' did not resolve")),
            "warnings: {:?}",
            resp.warnings
        );
    }
}
