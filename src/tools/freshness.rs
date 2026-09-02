//! Freshness assembly: verify the anchors backing a set of claims, derive each
//! claim's three-state disposition, and package a per-view `FreshnessManifest`.
//!
//! Also keeps the original repo-wide `stale_index_warning`, now one input among
//! several.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::domain::anchors::content_hash;
use crate::domain::completeness::uncovered;
use crate::domain::entities::{
    AnchorVerification, Claim, Disposition, EvidenceAnchor, Freshness, FreshnessManifest,
    IncompleteClaim, LaneStatus, RebuildObligation, StaleAnchorDetail, StaleClaim,
};
use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::verify_evidence::{resolve_head, verify_anchor, HeadRef};

// ---------------------------------------------------------------------------
// Verify mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    Cached,
    Fresh,
    Skip,
    Strict,
}

impl VerifyMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("fresh") => VerifyMode::Fresh,
            Some("skip") | Some("none") | Some("off") => VerifyMode::Skip,
            Some("strict") => VerifyMode::Strict,
            _ => VerifyMode::Cached,
        }
    }

    fn always_reverify(self) -> bool {
        matches!(self, VerifyMode::Fresh | VerifyMode::Strict)
    }
}

// ---------------------------------------------------------------------------
// Per-anchor resolution
// ---------------------------------------------------------------------------

pub struct AnchorState {
    pub anchor: EvidenceAnchor,
    pub verification: AnchorVerification,
}

/// Fetch claims + anchors for `claim_ids` and resolve every anchor's verification
/// against the working tree, honouring `mode`.
pub async fn resolve_claim_states(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
    head: &HeadRef,
    claim_ids: &[Uuid],
    mode: VerifyMode,
    now: &str,
) -> Result<Vec<(Claim, Vec<AnchorState>)>, Error> {
    let claims = store.claims_by_ids(claim_ids).await?;
    let anchors = store.anchors_for_claims(claim_ids).await?;

    let mut by_claim: BTreeMap<String, Vec<EvidenceAnchor>> = BTreeMap::new();
    for a in anchors {
        by_claim.entry(a.claim_id.to_string()).or_default().push(a);
    }

    let mut out = Vec::with_capacity(claims.len());
    for claim in claims {
        let mut states = Vec::new();
        for anchor in by_claim.remove(&claim.id.to_string()).unwrap_or_default() {
            let cached = if mode.always_reverify() {
                None
            } else {
                store
                    .latest_verification(anchor.id, &head.repo_commit)
                    .await?
            };
            let verification = match cached {
                Some(v) => v,
                None => {
                    let v = verify_anchor(store, repo_id, repo_path, head, &anchor, now).await;
                    // don't re-append an identical latest row
                    let dup = store
                        .latest_verification(anchor.id, &head.repo_commit)
                        .await?
                        .map(|prev| {
                            prev.freshness == v.freshness && prev.edit_class == v.edit_class
                        })
                        .unwrap_or(false);
                    if !dup {
                        store.insert_anchor_verification(&v).await?;
                    }
                    v
                }
            };
            states.push(AnchorState { anchor, verification });
        }
        out.push((claim, states));
    }
    Ok(out)
}

/// Three-state disposition of a claim over its resolved anchors.
pub fn claim_disposition(states: &[AnchorState]) -> Disposition {
    if states.is_empty() {
        return Disposition::Unprovable;
    }
    if states
        .iter()
        .all(|s| s.verification.freshness == Freshness::Fresh)
    {
        return Disposition::Unaffected;
    }
    let all_stale_relocated = states
        .iter()
        .filter(|s| s.verification.freshness == Freshness::Stale)
        .all(|s| s.verification.relocated_locator.is_some());
    if all_stale_relocated {
        Disposition::Affected
    } else {
        Disposition::Unprovable
    }
}

// ---------------------------------------------------------------------------
// Manifest assembly
// ---------------------------------------------------------------------------

/// Build the per-view freshness manifest for a set of claims. Returns `None`
/// only when `mode == Skip`.
pub async fn build_manifest(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
    tool: &str,
    view_hash: &str,
    claim_ids: &[Uuid],
    mode: VerifyMode,
) -> Result<Option<FreshnessManifest>, Error> {
    if mode == VerifyMode::Skip {
        return Ok(None);
    }
    let now = Utc::now().to_rfc3339();
    let head = resolve_head(repo_path);
    let states = resolve_claim_states(
        store, repo_id, repo_path, &head, claim_ids, mode, &now,
    )
    .await?;

    let mut by_disposition: BTreeMap<String, usize> =
        BTreeMap::from([("unaffected".into(), 0), ("affected".into(), 0), ("unprovable".into(), 0)]);
    let mut anchors_total = 0usize;
    let mut stale_claims = Vec::new();
    let mut incomplete_claims = Vec::new();

    for (claim, anchor_states) in &states {
        anchors_total += anchor_states.len();
        let disposition = claim_disposition(anchor_states);
        *by_disposition
            .entry(disposition.as_str().to_string())
            .or_insert(0) += 1;

        let missing = uncovered(
            &claim.read_set,
            &anchor_states.iter().map(|s| s.anchor.clone()).collect::<Vec<_>>(),
        );
        if !missing.is_empty() {
            incomplete_claims.push(IncompleteClaim {
                claim_id: claim.id.to_string(),
                text: claim.text.clone(),
                uncovered: missing,
            });
        }

        if disposition != Disposition::Unaffected {
            let (decision_id, adr_id) = store
                .decision_and_adr_for_subject(&claim.subject_type, claim.subject_id)
                .await?;
            let anchors = anchor_states
                .iter()
                .filter(|s| s.verification.freshness == Freshness::Stale || anchor_states.is_empty())
                .map(|s| StaleAnchorDetail {
                    anchor_id: s.anchor.id.to_string(),
                    identity: s.anchor.identity.clone(),
                    edit_class: s.verification.edit_class.as_str().to_string(),
                    freshness: s.verification.freshness.as_str().to_string(),
                    relocated_locator: s.verification.relocated_locator.clone(),
                    detail: s.verification.detail.clone(),
                })
                .collect();
            stale_claims.push(StaleClaim {
                claim_id: claim.id.to_string(),
                subject_type: claim.subject_type.clone(),
                subject_id: claim.subject_id.to_string(),
                decision_id,
                adr_id,
                text: claim.text.clone(),
                evidence_grade: claim.evidence_grade.as_str().to_string(),
                disposition: disposition.as_str().to_string(),
                anchors,
            });
        }
    }

    let mut warnings = Vec::new();
    if let Some(w) = stale_index_warning(store, repo_id, repo_path).await {
        warnings.push(w);
    }

    let manifest = FreshnessManifest {
        evaluated_at: now.clone(),
        repo_ref: head.repo_ref.clone(),
        repo_commit: head.repo_commit.clone(),
        anchors_total,
        by_disposition,
        stale_claims,
        incomplete_claims,
        lanes: lane_statuses(store, repo_id, repo_path).await?,
        warnings,
    };

    if let Ok(payload) = serde_json::to_string(&manifest) {
        let _ = store
            .store_freshness_manifest(
                repo_id,
                tool,
                view_hash,
                &head.repo_commit,
                &now,
                &payload,
            )
            .await;
    }

    Ok(Some(manifest))
}

/// Attach a freshness manifest to a retrieval response, scoped to the decisions
/// it already carries. In `strict` mode, also sets `response.refused` when a
/// stale claim is on the path.
pub async fn attach_manifest(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
    tool: &str,
    view_key: &str,
    response: &mut crate::domain::entities::ArchResponse,
    mode: VerifyMode,
) -> Result<(), Error> {
    if mode == VerifyMode::Skip {
        response
            .warnings
            .push("freshness verification skipped (verify: skip)".to_string());
        return Ok(());
    }
    let decision_ids: Vec<Uuid> = response
        .decisions
        .iter()
        .filter_map(|d| Uuid::parse_str(&d.id).ok())
        .collect();
    if decision_ids.is_empty() {
        return Ok(());
    }
    let claim_ids = store.claim_ids_for_decisions(&decision_ids).await?;
    let manifest = build_manifest(
        store,
        repo_id,
        repo_path,
        tool,
        &view_hash(tool, view_key),
        &claim_ids,
        mode,
    )
    .await?;

    if let Some(m) = manifest {
        if mode == VerifyMode::Strict {
            if let Some(obligation) = rebuild_obligation(&m, repo_path) {
                response.answer = Some(format!("refused: {}", obligation.reason));
                response.refused = Some(obligation);
            }
        }
        response.freshness = Some(m);
    }
    Ok(())
}

/// Build a strict-mode rebuild obligation from a manifest, or `None` if every
/// claim on the view is `unaffected`.
pub fn rebuild_obligation(
    manifest: &FreshnessManifest,
    repo_path: &Path,
) -> Option<RebuildObligation> {
    if manifest.stale_claims.is_empty() {
        return None;
    }
    let repo = repo_path.display().to_string();
    let mut commands = Vec::new();
    let kinds: Vec<&str> = manifest
        .stale_claims
        .iter()
        .flat_map(|c| c.anchors.iter().map(|a| a.identity.source_kind.as_str()))
        .collect();
    if kinds.iter().any(|k| *k == "adr") {
        commands.push(format!(
            "sync_adrs_from_git {{ \"repo_path\": \"{repo}\", \"adr_glob\": \"docs/adr/*.md\" }}"
        ));
    }
    if kinds.iter().any(|k| *k == "source_file" || *k == "symbol") {
        commands.push(format!(
            "sync_incremental {{ \"repo_path\": \"{repo}\", \"since\": \"{}\" }}",
            manifest.repo_commit
        ));
    }
    if kinds.iter().any(|k| *k == "commit") {
        commands.push(format!(
            "sync_commits_from_git {{ \"repo_path\": \"{repo}\" }}"
        ));
    }
    let n_unprovable = manifest
        .stale_claims
        .iter()
        .filter(|c| c.disposition == "unprovable")
        .count();
    let n_affected = manifest.stale_claims.len() - n_unprovable;
    Some(RebuildObligation {
        reason: format!(
            "{n_unprovable} claim(s) on the answer path are unprovable, {n_affected} affected"
        ),
        drifted_anchors: manifest
            .stale_claims
            .iter()
            .flat_map(|c| c.anchors.iter().map(|a| a.anchor_id.clone()))
            .collect(),
        commands,
    })
}

// ---------------------------------------------------------------------------
// Lane manifest
// ---------------------------------------------------------------------------

/// Capabilities a lane enables when its status is `ok`.
pub fn lane_capabilities(lane: &str) -> Vec<String> {
    let caps: &[&str] = match lane {
        "adr" => &["governance_lookup", "consistency_check"],
        "symbol" => &["symbol_lookup", "call_path", "anchor_verification"],
        "commit" => &["commit_evidence", "stale_decision_activity"],
        "embedding" => &["semantic_query"],
        "community" => &["architecture_communities"],
        "route" => &["route_lookup"],
        _ => &[],
    };
    caps.iter().map(|s| s.to_string()).collect()
}

pub async fn lane_statuses(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
) -> Result<Vec<LaneStatus>, Error> {
    let lanes = store.index_lanes(repo_id).await?;
    Ok(lanes
        .into_iter()
        .map(|l| {
            let lag = l
                .last_ingested_commit
                .as_deref()
                .and_then(|c| lag_commits(repo_path, c));
            let capabilities = if l.status == "ok" {
                lane_capabilities(&l.lane)
            } else {
                vec![]
            };
            LaneStatus {
                lane: l.lane,
                last_ingested_commit: l.last_ingested_commit,
                lag_commits: lag,
                status: l.status,
                capabilities,
            }
        })
        .collect())
}

/// Current `HEAD` commit sha, or `None` when the path is not a git repo.
pub fn head_commit(repo_path: &Path) -> Option<String> {
    let h = resolve_head(repo_path);
    if h.repo_commit == "working-tree" {
        None
    } else {
        Some(h.repo_commit)
    }
}

/// Record an index lane's freshness against the current HEAD. Best-effort:
/// ingestion should not fail because lane bookkeeping did.
pub async fn record_lane(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
    lane: &str,
    status: &str,
) {
    let now = Utc::now().to_rfc3339();
    let commit = head_commit(repo_path);
    let _ = store
        .upsert_index_lane(repo_id, lane, commit.as_deref(), &now, status, None)
        .await;
}

/// `git rev-list --count <commit>..HEAD`. `None` when git is unavailable.
pub fn lag_commits(repo_path: &Path, commit: &str) -> Option<u32> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-list")
        .arg("--count")
        .arg(format!("{commit}..HEAD"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Legacy repo-wide staleness warning
// ---------------------------------------------------------------------------

/// Returns a warning string if the symbol index appears stale relative to the
/// current git HEAD. Returns None when fresh, when git is unavailable (not a
/// git repo, bare clone, etc.), or when no files have been indexed yet.
pub async fn stale_index_warning(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &Path,
) -> Option<String> {
    let last_ingested_str = store.last_file_ingested_at(repo_id).await.ok()??;

    let last_ingested_secs = chrono::DateTime::parse_from_rfc3339(&last_ingested_str)
        .ok()?
        .timestamp();

    let head_secs = git_head_timestamp_secs(repo_path)?;

    if head_secs > last_ingested_secs {
        Some(format!(
            "symbol index may be stale (last ingested {}); \
             git HEAD is newer — run sync_incremental or ingest_symbols to refresh",
            &last_ingested_str[..10],
        ))
    } else {
        None
    }
}

fn git_head_timestamp_secs(repo_path: &Path) -> Option<i64> {
    let repo = git2::Repository::open(repo_path).ok()?;
    let commit = repo.head().ok()?.peel_to_commit().ok()?;
    Some(commit.time().seconds())
}

/// sha256 hex helper for view-hash computation by callers.
pub fn view_hash(tool: &str, canonical_args: &str) -> String {
    content_hash(&format!("{tool}\u{1f}{canonical_args}"))
}
