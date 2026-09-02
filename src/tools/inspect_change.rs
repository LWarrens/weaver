use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::entities::{DecisionSummary, TemporalContext};
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectChangeParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Paths of changed files, relative to the repository root.
    pub files: Vec<String>,
    /// Optional human-readable summary of the change. Summary terms are checked
    /// against active constraints alongside file path terms.
    #[serde(default)]
    pub change_summary: Option<String>,
    /// Optional symbol names touched by the change. Their containing files will
    /// also be checked against constraints.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
    /// Freshness verification mode for the evidence behind the flagged
    /// decisions. Defaults to `strict`: if a decision on the path has drifted
    /// evidence, the tool refuses with a rebuild obligation rather than letting
    /// a change land against a decision it can no longer prove.
    #[serde(default)]
    pub verify: Option<String>,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct InspectChangeResult {
    pub possible_violations: Vec<PossibleViolation>,
    pub relevant_decisions: Vec<DecisionSummary>,
    /// Freshness of the evidence behind the flagged decisions. `None` when
    /// `verify: skip` or nothing was flagged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::domain::entities::FreshnessManifest>,
    /// Present when `verify: strict` (the default) found drifted evidence on a
    /// flagged decision — act on the rebuild obligation before trusting the
    /// violation check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<crate::domain::entities::RebuildObligation>,
    pub warnings: Vec<String>,
    pub temporal_context: TemporalContext,
}

#[derive(Debug, Serialize)]
pub struct PossibleViolation {
    pub decision_id: String,
    pub adr_id: String,
    pub constraint: String,
    pub matched_files: Vec<String>,
    pub matched_summary: bool,
    pub confidence: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: InspectChangeParams,
) -> Result<InspectChangeResult, Error> {
    let now = Utc::now().to_rfc3339();

    // --- Validation ---------------------------------------------------------
    if params.files.is_empty() && params.symbols.is_empty() {
        return Err(Error::InvalidInput {
            field: "files",
            reason: "at least one file or symbol must be provided".to_string(),
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

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    let valid_at = params.valid_at.as_deref();
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let mut warnings: Vec<String> = vec![];

    // --- Build the full set of changed files --------------------------------
    // Start with explicitly listed files, then expand via symbol lookup.
    let mut changed_files: Vec<String> =
        params.files.iter().map(|f| f.replace('\\', "/")).collect();

    for symbol in &params.symbols {
        let symbol_files = store
            .find_files_with_symbol(repo.id, symbol, valid_at)
            .await?;
        if symbol_files.is_empty() {
            warnings.push(format!(
                "symbol '{}' not found in symbol index — run ingest_symbols first",
                symbol
            ));
        }
        for f in symbol_files {
            if !changed_files.contains(&f) {
                changed_files.push(f);
            }
        }
    }

    // --- Expand via call graph (if available) -------------------------------
    // Find all files that transitively CALL INTO the changed files (callers).
    // These callers may also be affected by the change, so include them in the
    // constraint check. Use up to 3 hops to bound the traversal.
    if store.has_symbol_edges(repo.id).await? {
        let mut frontier = changed_files.clone();
        for _ in 0..3 {
            let callers = store
                .find_one_hop_caller_files(repo.id, &frontier)
                .await?;
            let mut added = false;
            for f in callers {
                if !changed_files.contains(&f) {
                    changed_files.push(f.clone());
                    frontier.push(f);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }

    // --- Load ALL active decisions for this repo ----------------------------
    // Constraint violation detection compares every active constraint against
    // the changed file paths via keyword overlap — not only decisions that have
    // explicit file links. This matches the intent from the TypeScript prototype.
    let all_decisions = store.list_all_decisions(repo.id, valid_at).await?;

    if all_decisions.is_empty() {
        warnings.push(
            "no decisions found for this repository; sync ADRs first with sync_adrs_from_git"
            .to_string(),
        );
        return Ok(InspectChangeResult {
            possible_violations: vec![],
            relevant_decisions: vec![],
            freshness: None,
            refused: None,
            warnings,
            temporal_context: TemporalContext {
                valid_at: params.valid_at,
                ingested_at: Some(now),
            },
        });
    }

    // --- Fetch constraints for all decisions --------------------------------
    let decision_ids: Vec<String> = all_decisions.iter().map(|d| d.id.clone()).collect();
    let constraints = store.find_constraints_for_decisions(&decision_ids).await?;

    // Build a fast decision lookup by id
    let decision_map: std::collections::HashMap<String, &crate::domain::entities::DecisionSummary> =
        all_decisions.iter().map(|d| (d.id.clone(), d)).collect();

    // --- Detect violations via keyword-overlap matching ---------------------
    let mut violation_map: HashMap<(String, String), PossibleViolation> = HashMap::new();

    for constraint in &constraints {
        let constraint_words: Vec<String> = constraint
            .text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            .map(str::to_string)
            .collect();

        let mut matched_files: Vec<String> = vec![];
        let matched_summary = params
            .change_summary
            .as_deref()
            .map(|summary| text_matches_constraint(summary, &constraint_words))
            .unwrap_or(false);

        for file in &changed_files {
            let file_lc = file.to_lowercase();
            let segments: Vec<&str> = file_lc.split('/').collect();
            let file_matches = constraint_words
                .iter()
                .any(|word| segments.iter().any(|seg| seg.contains(word.as_str())));
            if file_matches {
                matched_files.push(file.clone());
            }
        }

        if matched_files.is_empty() && !matched_summary {
            continue;
        }

        let decision = decision_map.get(&constraint.decision_id);
        let (adr_id, is_accepted) = match decision {
            Some(d) => (d.adr_id.clone(), d.status.eq_ignore_ascii_case("accepted")),
            None => (constraint.decision_id.clone(), false),
        };

        let confidence = if constraint_words.is_empty() {
            "low".to_string()
        } else if matched_summary && matched_files.is_empty() {
            "medium".to_string()
        } else if is_accepted {
            "high".to_string()
        } else {
            "medium".to_string()
        };

        let key = (constraint.decision_id.clone(), constraint.text.clone());
        violation_map.entry(key).or_insert(PossibleViolation {
            decision_id: constraint.decision_id.clone(),
            adr_id,
            constraint: constraint.text.clone(),
            matched_files,
            matched_summary,
            confidence,
        });
    }

    let possible_violations: Vec<PossibleViolation> = violation_map.into_values().collect();

    // Collect only the decisions that had at least one constraint checked (all of them)
    let relevant_decisions: Vec<DecisionSummary> = all_decisions;

    // --- Freshness of the flagged decisions --------------------------------
    // Strict by default: if a flagged decision's evidence has drifted, refuse
    // rather than let a change land against a decision we cannot prove.
    let mode = crate::tools::freshness::VerifyMode::parse(Some(
        params.verify.as_deref().unwrap_or("strict"),
    ));
    let mut freshness = None;
    let mut refused = None;
    if mode != crate::tools::freshness::VerifyMode::Skip {
        let flagged_ids: Vec<uuid::Uuid> = possible_violations
            .iter()
            .filter_map(|v| uuid::Uuid::parse_str(&v.decision_id).ok())
            .collect();
        if !flagged_ids.is_empty() {
            let claim_ids = store.claim_ids_for_decisions(&flagged_ids).await?;
            let view = crate::tools::freshness::view_hash(
                "inspect_change_against_decisions",
                &changed_files.join(","),
            );
            if let Some(m) = crate::tools::freshness::build_manifest(
                &store, repo.id, &repo_path, "inspect_change_against_decisions", &view,
                &claim_ids, mode,
            )
            .await?
            {
                if mode == crate::tools::freshness::VerifyMode::Strict {
                    refused = crate::tools::freshness::rebuild_obligation(&m, &repo_path);
                }
                freshness = Some(m);
            }
        }
    }

    Ok(InspectChangeResult {
        possible_violations,
        relevant_decisions,
        freshness,
        refused,
        warnings,
        temporal_context: TemporalContext {
            valid_at: params.valid_at,
            ingested_at: Some(now),
        },
    })
}

fn text_matches_constraint(text: &str, constraint_words: &[String]) -> bool {
    let lower = text.to_lowercase();
    let summary_words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .collect();

    constraint_words.iter().any(|word| {
        summary_words
            .iter()
            .any(|summary_word| summary_word.contains(word))
    })
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

    const AUTH_ADR: &str = r#"# ADR-0010 Authentication must use JWT tokens

## Status

Accepted

Date: 2024-01-15.

## Context

We need stateless auth.

## Decision

Use JWT tokens signed with RS256. The file `src/auth/login.rs` must implement token validation.

## Consequences

- Stateless authentication.

## Constraints

- All auth flows must use JWT RS256 tokens.
- No session cookies in API services.
"#;

    const PROPOSED_AUTH_ADR: &str = r#"# ADR-0011 Authentication may use opaque API tokens

## Status

Proposed

Date: 2024-01-16.

## Context

We are evaluating service tokens.

## Decision

Internal auth flows must use opaque API tokens.

## Consequences

- Token introspection may be required.

## Constraints

- Internal auth flows must use opaque API tokens.
"#;

    #[tokio::test]
    async fn detects_violation_for_auth_file() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("ADR-0010.md"), AUTH_ADR).expect("write");

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
            InspectChangeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                files: vec!["src/auth/session.rs".to_string()],
                change_summary: None,
                symbols: vec![],
                valid_at: None,
                verify: None,
            },
        )
        .await
        .expect("inspect");

        assert!(
            !result.possible_violations.is_empty(),
            "expected a violation for src/auth/session.rs; warnings: {:?}",
            result.warnings
        );
        assert_eq!(result.possible_violations[0].adr_id, "ADR-0010");
        assert_eq!(result.possible_violations[0].confidence, "high");
    }

    #[tokio::test]
    async fn proposed_adr_violation_has_medium_confidence() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("ADR-0011.md"), PROPOSED_AUTH_ADR).expect("write");

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
            InspectChangeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                files: vec!["src/auth/tokens.rs".to_string()],
                change_summary: None,
                symbols: vec![],
                valid_at: None,
                verify: None,
            },
        )
        .await
        .expect("inspect");

        let violation = result
            .possible_violations
            .iter()
            .find(|v| v.adr_id == "ADR-0011")
            .expect("expected proposed ADR violation");

        assert_eq!(violation.confidence, "medium");
    }

    #[tokio::test]
    async fn no_violation_for_unrelated_file() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("ADR-0010.md"), AUTH_ADR).expect("write");

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
            InspectChangeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                files: vec!["src/billing/invoice.rs".to_string()],
                change_summary: None,
                symbols: vec![],
                valid_at: None,
                verify: None,
            },
        )
        .await
        .expect("inspect");

        assert!(
            result.possible_violations.is_empty(),
            "expected no violations for billing file; got: {:?}",
            result.possible_violations
        );
    }

    #[tokio::test]
    async fn strict_mode_refuses_when_flagged_decision_evidence_drifted() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr_path = dir.path().join("ADR-0010.md");
        std::fs::write(&adr_path, AUTH_ADR).expect("write");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        // Rewrite the Decision section on disk without re-syncing: the decision
        // claim's anchor can no longer be found.
        std::fs::write(
            &adr_path,
            AUTH_ADR.replace(
                "Use JWT tokens signed with RS256. The file `src/auth/login.rs` must implement token validation.",
                "Use a third-party identity provider for all authentication.",
            ),
        )
        .expect("rewrite");

        let result = run(
            &store,
            InspectChangeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                files: vec!["src/auth/session.rs".to_string()],
                change_summary: None,
                symbols: vec![],
                valid_at: None,
                verify: None, // defaults to strict
            },
        )
        .await
        .expect("inspect");

        assert!(
            result.refused.is_some(),
            "strict mode should refuse; freshness: {:?}",
            result.freshness
        );
        assert!(!result.refused.unwrap().commands.is_empty());
    }

    #[tokio::test]
    async fn detects_violation_from_change_summary() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("ADR-0010.md"), AUTH_ADR).expect("write");

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
            InspectChangeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                files: vec!["src/runtime/session.rs".to_string()],
                change_summary: Some("Replace JWT auth flow with opaque API tokens".to_string()),
                symbols: vec![],
                valid_at: None,
                verify: None,
            },
        )
        .await
        .expect("inspect");

        let violation = result
            .possible_violations
            .iter()
            .find(|v| v.adr_id == "ADR-0010")
            .expect("expected summary-based violation");

        assert!(violation.matched_summary);
        assert_eq!(violation.confidence, "medium");
    }
}
