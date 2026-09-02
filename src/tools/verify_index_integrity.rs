//! Offline integrity oracle for the ADR index lane.
//!
//! CodeNib's ℱ(G) = ℱ(G) check (docs/DESIGN-claims-and-freshness.md, Step 8):
//! the declared-fact projection produced by incremental `sync_adrs_from_git`
//! must equal the projection produced by a full re-sync from scratch. This tool
//! rebuilds the ADR lane in a throwaway in-memory store and diffs the two
//! projections. It is deliberately off the hot path — run it in CI or manually,
//! never inside a query.
//!
//! Only the ADR lane is audited here: it is the one lane whose incremental and
//! full paths share no code and can therefore diverge. Symbol/commit lanes are
//! reported as `not_audited`.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::anchors::normalize_ws;
use crate::domain::entities::{AnchorSource, Claim};
use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::sync_adrs::{self, SyncAdrsFromGitParams};

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyIndexIntegrityParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Glob for ADR files relative to the repo root. Default: `docs/adr/*.md`.
    #[serde(default = "default_adr_glob")]
    pub adr_glob: String,
}

fn default_adr_glob() -> String {
    "docs/adr/*.md".to_string()
}

#[derive(Debug, Serialize)]
pub struct LaneIntegrity {
    pub lane: String,
    /// `clean`, `divergent`, or `not_audited`.
    pub status: String,
    pub live_claims: usize,
    pub rebuilt_claims: usize,
    /// Projection keys present live but not in a full re-sync (stale rows).
    pub only_in_live: Vec<String>,
    /// Projection keys a full re-sync produces that the live index is missing.
    pub only_in_rebuild: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyIndexIntegrityResult {
    pub repo_path: String,
    /// True when every audited lane is `clean`.
    pub consistent: bool,
    pub lanes: Vec<LaneIntegrity>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

/// A stable, order-independent fingerprint of one claim and its ADR anchors.
fn projection_key(claim: &Claim, anchor_hashes: &[String]) -> String {
    let mut hashes: Vec<&str> = anchor_hashes.iter().map(String::as_str).collect();
    hashes.sort_unstable();
    format!(
        "{}|{}|{}|{}",
        claim.kind.as_str(),
        claim.polarity.map(|p| p.as_str()).unwrap_or("none"),
        normalize_ws(&claim.text),
        hashes.join(","),
    )
}

/// Build the multiset of projection keys for every open ADR-sourced claim.
async fn adr_projection(
    store: &SqliteStore,
    repo_id: uuid::Uuid,
) -> Result<BTreeMap<String, usize>, Error> {
    let claims = store.open_claims_for_repo(repo_id).await?;
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for claim in &claims {
        let anchors = store.anchors_for_claim(claim.id).await?;
        let adr_hashes: Vec<String> = anchors
            .iter()
            .filter(|a| a.identity.source_kind == AnchorSource::Adr)
            .map(|a| a.content_hash.clone())
            .collect();
        if adr_hashes.is_empty() {
            continue; // not an ADR-lane claim
        }
        *out.entry(projection_key(claim, &adr_hashes)).or_insert(0) += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: VerifyIndexIntegrityParams,
) -> Result<VerifyIndexIntegrityResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;
    let live = adr_projection(store, repo.id).await?;

    // Full re-sync into a throwaway store.
    let scratch = SqliteStore::connect("sqlite::memory:").await?;
    scratch.run_migrations().await?;
    let scratch = Arc::new(scratch);
    let mut warnings: Vec<String> = Vec::new();
    let scratch_repo = scratch.upsert_repository(repo_path_str, None).await?;

    if let Err(e) = sync_adrs::run(
        &scratch,
        SyncAdrsFromGitParams {
            repo_path: repo_path_str.to_string(),
            adr_glob: params.adr_glob.clone(),
        },
    )
    .await
    {
        warnings.push(format!("full re-sync failed: {e}"));
    }
    let rebuilt = adr_projection(&scratch, scratch_repo.id).await?;

    let only_in_live: Vec<String> = live
        .keys()
        .filter(|k| !rebuilt.contains_key(*k))
        .cloned()
        .collect();
    let only_in_rebuild: Vec<String> = rebuilt
        .keys()
        .filter(|k| !live.contains_key(*k))
        .cloned()
        .collect();

    let adr_clean = only_in_live.is_empty() && only_in_rebuild.is_empty();
    let lanes = vec![
        LaneIntegrity {
            lane: "adr".to_string(),
            status: if adr_clean { "clean" } else { "divergent" }.to_string(),
            live_claims: live.values().sum(),
            rebuilt_claims: rebuilt.values().sum(),
            only_in_live,
            only_in_rebuild,
        },
        LaneIntegrity {
            lane: "symbol".to_string(),
            status: "not_audited".to_string(),
            live_claims: 0,
            rebuilt_claims: 0,
            only_in_live: vec![],
            only_in_rebuild: vec![],
        },
        LaneIntegrity {
            lane: "commit".to_string(),
            status: "not_audited".to_string(),
            live_claims: 0,
            rebuilt_claims: 0,
            only_in_live: vec![],
            only_in_rebuild: vec![],
        },
    ];

    Ok(VerifyIndexIntegrityResult {
        repo_path: repo_path_str.to_string(),
        consistent: adr_clean,
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
    use std::fs;
    use tempfile::tempdir;

    async fn store() -> Arc<SqliteStore> {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        s.run_migrations().await.unwrap();
        Arc::new(s)
    }

    #[tokio::test]
    async fn clean_after_incremental_sync() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr")).unwrap();
        git2::Repository::init(&repo).unwrap();
        fs::write(
            repo.join("docs/adr/0001-x.md"),
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nServices must use the bus.\n",
        )
        .unwrap();

        let store = store().await;
        sync_adrs::run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await
        .unwrap();

        let res = run(
            &store,
            VerifyIndexIntegrityParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(res.consistent, "lanes: {:#?}", res.lanes);
        let adr = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
        assert_eq!(adr.status, "clean");
        assert!(adr.live_claims > 0);
    }

    #[tokio::test]
    async fn detects_stale_claim_after_adr_edit_without_resync() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr")).unwrap();
        git2::Repository::init(&repo).unwrap();
        let adr = repo.join("docs/adr/0001-x.md");
        fs::write(
            &adr,
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse approach one.\n",
        )
        .unwrap();

        let store = store().await;
        sync_adrs::run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await
        .unwrap();

        // Edit the ADR but do NOT re-sync — live index now stale.
        fs::write(
            &adr,
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse approach two instead.\n",
        )
        .unwrap();

        let res = run(
            &store,
            VerifyIndexIntegrityParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(!res.consistent, "expected divergence, lanes: {:#?}", res.lanes);
        let adr_lane = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
        assert_eq!(adr_lane.status, "divergent");
        assert!(!adr_lane.only_in_live.is_empty());
        assert!(!adr_lane.only_in_rebuild.is_empty());
    }
}
