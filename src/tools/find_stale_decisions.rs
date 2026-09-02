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
pub struct FindStaleDecisionsParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// ISO-8601 timestamp or git ref. Only decisions with no linked commit activity
    /// after this date are flagged by the "no recent activity" heuristic.
    /// Defaults to 180 days ago.
    #[serde(default)]
    pub since: Option<String>,
    /// Minimum confidence to include a candidate in results. Default: 0.0.
    #[serde(default)]
    pub min_confidence: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct StalenessSignal {
    /// Signal kind: "deleted_file", "missing_symbols", "aged_adr",
    /// "no_recent_activity", "drifted_evidence".
    pub kind: String,
    pub detail: String,
    pub confidence: f64,
}

#[derive(Debug, Serialize)]
pub struct StaleDecision {
    pub decision_id: String,
    pub adr_id: String,
    pub title: String,
    pub valid_from: String,
    /// Aggregate confidence: max of individual signal confidences.
    pub confidence: f64,
    pub signals: Vec<StalenessSignal>,
}

#[derive(Debug, Serialize)]
pub struct FindStaleDecisionsResult {
    pub checked_at: String,
    pub since: String,
    pub stale_decisions: Vec<StaleDecision>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: FindStaleDecisionsParams,
) -> Result<FindStaleDecisionsResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let now = Utc::now();
    let checked_at = now.to_rfc3339();
    let min_confidence = params.min_confidence.unwrap_or(0.0);
    let mut warnings: Vec<String> = vec![];

    // Default "since" to 180 days ago
    let since = params.since.unwrap_or_else(|| {
        (now - chrono::Duration::days(180)).to_rfc3339()
    });

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // Collect signals per decision into a map
    use std::collections::HashMap;
    let mut map: HashMap<String, StaleDecision> = HashMap::new();

    // ------------------------------------------------------------------
    // Signal 1: decisions linked to files that have since been removed
    // ------------------------------------------------------------------
    let deleted_file_hits = store
        .stale_decisions_deleted_files(repo.id)
        .await?;

    for (decision_id, adr_id, title, valid_from, file_path) in deleted_file_hits {
        let signal = StalenessSignal {
            kind: "deleted_file".to_string(),
            detail: format!("linked file '{}' has been removed from the index", file_path),
            confidence: 0.85,
        };
        add_signal(&mut map, &decision_id, &adr_id, &title, &valid_from, signal);
    }

    // ------------------------------------------------------------------
    // Signal 2: decisions whose mentioned files have no live symbols
    // (all symbols gone after a re-ingest — likely refactored away)
    // ------------------------------------------------------------------
    let missing_symbol_hits = store
        .stale_decisions_missing_symbols(repo.id)
        .await?;

    for (decision_id, adr_id, title, valid_from, file_path) in missing_symbol_hits {
        let signal = StalenessSignal {
            kind: "missing_symbols".to_string(),
            detail: format!(
                "file '{}' has no live symbols — may have been refactored or removed",
                file_path
            ),
            confidence: 0.70,
        };
        add_signal(&mut map, &decision_id, &adr_id, &title, &valid_from, signal);
    }

    // ------------------------------------------------------------------
    // Signal 3: accepted ADRs with no linked commit activity since `since`
    // ------------------------------------------------------------------
    let no_activity_hits = store
        .stale_decisions_no_recent_activity(repo.id, &since)
        .await?;
    let no_activity_count = no_activity_hits.len();

    for (decision_id, adr_id, title, valid_from) in no_activity_hits {
        let signal = StalenessSignal {
            kind: "no_recent_activity".to_string(),
            detail: format!(
                "no commits linked to this decision since {} — may have drifted from implementation",
                since
            ),
            confidence: 0.45,
        };
        add_signal(&mut map, &decision_id, &adr_id, &title, &valid_from, signal);
    }

    // ------------------------------------------------------------------
    // Signal 4: very old accepted ADRs not superseded
    // ------------------------------------------------------------------
    let aged_hits = store
        .stale_decisions_aged(repo.id, &since)
        .await?;

    if no_activity_count == 0 && !aged_hits.is_empty() {
        warnings.push("aged_adr signal is advisory only — ADR age alone does not indicate staleness".to_string());
    }

    for (decision_id, adr_id, title, valid_from) in aged_hits {
        let signal = StalenessSignal {
            kind: "aged_adr".to_string(),
            detail: format!(
                "accepted since {} and not superseded — consider reviewing for continued relevance",
                valid_from
            ),
            confidence: 0.35,
        };
        add_signal(&mut map, &decision_id, &adr_id, &title, &valid_from, signal);
    }

    // ------------------------------------------------------------------
    // Signal 5: drifted evidence — an anchor behind one of the decision's
    // claims failed its most recent verification (docs/DESIGN-claims-and-
    // freshness.md). Relocated-only drift is "affected" (0.6); drift that
    // could not be relocated is "unprovable" (0.8).
    // ------------------------------------------------------------------
    for (decision_id, adr_id, title, valid_from, stale_n, unreloc_n) in
        store.drifted_evidence_decisions(repo.id).await?
    {
        let (confidence, disposition) = if unreloc_n > 0 {
            (0.8, "unprovable")
        } else {
            (0.6, "affected")
        };
        let signal = StalenessSignal {
            kind: "drifted_evidence".to_string(),
            detail: format!(
                "{stale_n} evidence anchor(s) failed re-verification ({disposition}); \
                 re-run verify_claims and update the ADR if the code has moved on"
            ),
            confidence,
        };
        add_signal(&mut map, &decision_id, &adr_id, &title, &valid_from, signal);
    }

    // ------------------------------------------------------------------
    // Filter and sort
    // ------------------------------------------------------------------
    let mut stale_decisions: Vec<StaleDecision> = map
        .into_values()
        .filter(|d| d.confidence >= min_confidence)
        .collect();

    stale_decisions
        .sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));

    if stale_decisions.is_empty() {
        warnings.push(
            "no stale decisions detected — run ingest_symbols and sync_commits_from_git first for full coverage".to_string(),
        );
    }

    Ok(FindStaleDecisionsResult {
        checked_at,
        since,
        stale_decisions,
        warnings,
    })
}

fn add_signal(
    map: &mut std::collections::HashMap<String, StaleDecision>,
    decision_id: &str,
    adr_id: &str,
    title: &str,
    valid_from: &str,
    signal: StalenessSignal,
) {
    let entry = map
        .entry(decision_id.to_string())
        .or_insert_with(|| StaleDecision {
            decision_id: decision_id.to_string(),
            adr_id: adr_id.to_string(),
            title: title.to_string(),
            valid_from: valid_from.to_string(),
            confidence: 0.0,
            signals: vec![],
        });
    if signal.confidence > entry.confidence {
        entry.confidence = signal.confidence;
    }
    entry.signals.push(signal);
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
    async fn find_stale_returns_ok_on_fresh_repo() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo)?;
        git2::Repository::init(&repo)?;

        let content = "# ADR-0001: Use SQLite\n\n## Status\nAccepted\n\n## Context\nStorage.\n\n## Decision\nUse SQLite.\n\n## Consequences\nMust not use Postgres.\n";
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

        let result = run(
            &store,
            FindStaleDecisionsParams {
                repo_path: repo.to_string_lossy().to_string(),
                since: None,
                min_confidence: None,
            },
        )
        .await?;

        // Result must be a valid struct regardless of whether decisions are flagged
        assert!(!result.checked_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn drifted_evidence_signal_after_adr_edit() -> Result<(), Box<dyn std::error::Error>> {
        use crate::tools::verify_claims::{self, VerifyClaimsParams};

        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr"))?;
        git2::Repository::init(&repo)?;
        let adr = repo.join("docs/adr/0001-x.md");
        fs::write(
            &adr,
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse approach one for the pipeline.\n",
        )?;

        let store = make_store(&td).await;
        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await?;

        // Edit the Decision section without re-syncing, then verify — this
        // records a `stale` verification.
        fs::write(
            &adr,
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse a completely different approach now.\n",
        )?;
        verify_claims::run(
            &store,
            VerifyClaimsParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_id: Some("ADR-0001".to_string()),
                decision_id: None,
                file: None,
                symbol: None,
                verify: Some("fresh".to_string()),
            },
        )
        .await?;

        let result = run(
            &store,
            FindStaleDecisionsParams {
                repo_path: repo.to_string_lossy().to_string(),
                since: None,
                min_confidence: None,
            },
        )
        .await?;

        assert!(
            result
                .stale_decisions
                .iter()
                .flat_map(|d| &d.signals)
                .any(|s| s.kind == "drifted_evidence"),
            "expected a drifted_evidence signal: {:#?}",
            result.stale_decisions
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_repo_path_returns_error() {
        let store = {
            let td = tempdir().unwrap();
            let db_path = td.path().join("db.sqlite");
            let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
            let store = Arc::new(SqliteStore::connect(&db_url).await.unwrap());
            store.run_migrations().await.unwrap();
            store
        };

        let err = run(
            &store,
            FindStaleDecisionsParams {
                repo_path: "/nonexistent/path/xyz".to_string(),
                since: None,
                min_confidence: None,
            },
        )
        .await;

        assert!(err.is_err());
    }
}
