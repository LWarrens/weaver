//! `verify_claims` — full anchor / verification / disposition / completeness
//! detail for the claims of an ADR, decision, or file. The debugging counterpart
//! to the inline freshness manifest, as `explain_answer` is to `query`.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::entities::TemporalMode;
use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::freshness::{
    build_manifest, claim_disposition, resolve_claim_states, view_hash, VerifyMode,
};
use crate::tools::verify_evidence::resolve_head;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyClaimsParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Human ADR id, e.g. "ADR-0042".
    #[serde(default)]
    pub adr_id: Option<String>,
    /// Decision UUID.
    #[serde(default)]
    pub decision_id: Option<String>,
    /// Repo-relative file path.
    #[serde(default)]
    pub file: Option<String>,
    /// Symbol name.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Verification mode: `cached` (default), `fresh`, `skip`.
    #[serde(default)]
    pub verify: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifiedAnchor {
    pub anchor_id: String,
    pub source_kind: String,
    pub source_uri: String,
    pub subpath: String,
    pub edit_class: String,
    pub freshness: String,
    pub similarity: Option<f64>,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifiedClaim {
    pub claim_id: String,
    pub kind: String,
    pub subject_type: String,
    pub subject_id: String,
    pub text: String,
    pub evidence_grade: String,
    pub disposition: String,
    pub complete: bool,
    pub anchors: Vec<VerifiedAnchor>,
}

#[derive(Debug, Serialize)]
pub struct VerifyClaimsResult {
    pub target: String,
    pub repo_ref: String,
    pub repo_commit: String,
    pub claims: Vec<VerifiedClaim>,
    pub manifest: Option<crate::domain::entities::FreshnessManifest>,
    pub warnings: Vec<String>,
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: VerifyClaimsParams,
) -> Result<VerifyClaimsResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let mode = VerifyMode::parse(params.verify.as_deref());
    let mut warnings = Vec::new();

    let (target, claim_ids): (String, Vec<Uuid>) = if let Some(did) = &params.decision_id {
        let id = Uuid::parse_str(did).map_err(|_| Error::InvalidInput {
            field: "decision_id",
            reason: "not a UUID".to_string(),
        })?;
        (
            format!("decision:{did}"),
            store.claim_ids_for_decisions(&[id]).await?,
        )
    } else if let Some(adr) = &params.adr_id {
        match store.find_adr_by_adr_id(repo.id, adr).await? {
            Some(doc) => (
                format!("adr:{adr}"),
                store.claim_ids_for_adr_doc(doc.id).await?,
            ),
            None => {
                warnings.push(format!("ADR {adr} not found"));
                (format!("adr:{adr}"), vec![])
            }
        }
    } else if let Some(file) = &params.file {
        let decisions = store
            .find_decisions_for_file(repo.id, file, None, TemporalMode::Event)
            .await?;
        let ids = decision_uuids(&decisions);
        (
            format!("file:{file}"),
            store.claim_ids_for_decisions(&ids).await?,
        )
    } else if let Some(symbol) = &params.symbol {
        let files = store.find_files_with_symbol(repo.id, symbol, None).await?;
        let mut ids = Vec::new();
        for f in &files {
            let decisions = store
                .find_decisions_for_file(repo.id, f, None, TemporalMode::Event)
                .await?;
            ids.extend(decision_uuids(&decisions));
        }
        ids.sort();
        ids.dedup();
        (
            format!("symbol:{symbol}"),
            store.claim_ids_for_decisions(&ids).await?,
        )
    } else {
        return Err(Error::InvalidInput {
            field: "target",
            reason: "provide one of adr_id, decision_id, file, symbol".to_string(),
        });
    };

    let head = resolve_head(&repo_path);
    let now = chrono::Utc::now().to_rfc3339();
    let states = resolve_claim_states(
        store, repo.id, &repo_path, &head, &claim_ids, mode, &now,
    )
    .await?;

    let mut claims = Vec::new();
    for (claim, anchor_states) in &states {
        let disposition = claim_disposition(anchor_states);
        let anchor_view: Vec<_> = anchor_states
            .iter()
            .map(|s| s.anchor.clone())
            .collect();
        let complete =
            crate::domain::completeness::uncovered(&claim.read_set, &anchor_view).is_empty();
        claims.push(VerifiedClaim {
            claim_id: claim.id.to_string(),
            kind: format!("{:?}", claim.kind).to_lowercase(),
            subject_type: claim.subject_type.clone(),
            subject_id: claim.subject_id.to_string(),
            text: claim.text.clone(),
            evidence_grade: claim.evidence_grade.as_str().to_string(),
            disposition: disposition.as_str().to_string(),
            complete,
            anchors: anchor_states
                .iter()
                .map(|s| VerifiedAnchor {
                    anchor_id: s.anchor.id.to_string(),
                    source_kind: s.anchor.identity.source_kind.as_str().to_string(),
                    source_uri: s.anchor.identity.source_uri.clone(),
                    subpath: s.anchor.identity.subpath.clone(),
                    edit_class: s.verification.edit_class.as_str().to_string(),
                    freshness: s.verification.freshness.as_str().to_string(),
                    similarity: s.verification.similarity,
                    detail: s.verification.detail.clone(),
                })
                .collect(),
        });
    }

    let manifest = build_manifest(
        store,
        repo.id,
        &repo_path,
        "verify_claims",
        &view_hash("verify_claims", &target),
        &claim_ids,
        mode,
    )
    .await?;

    Ok(VerifyClaimsResult {
        target,
        repo_ref: head.repo_ref,
        repo_commit: head.repo_commit,
        claims,
        manifest,
        warnings,
    })
}

fn decision_uuids(decisions: &[crate::domain::entities::DecisionSummary]) -> Vec<Uuid> {
    decisions
        .iter()
        .filter_map(|d| Uuid::parse_str(&d.id).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    const ADR: &str = r#"# ADR-0001: Use event sourcing

## Status

Accepted

## Decision

The order service will use event sourcing. State is rebuilt by folding events.

## Consequences

- Order state must never be mutated in place.
"#;

    async fn sync(store: &Arc<SqliteStore>, dir: &std::path::Path) {
        crate::tools::sync_adrs::run(
            store,
            crate::tools::sync_adrs::SyncAdrsFromGitParams {
                repo_path: dir.to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");
    }

    fn params(dir: &std::path::Path) -> VerifyClaimsParams {
        VerifyClaimsParams {
            repo_path: dir.to_str().unwrap().to_string(),
            adr_id: Some("ADR-0001".to_string()),
            decision_id: None,
            file: None,
            symbol: None,
            verify: Some("fresh".to_string()),
        }
    }

    #[tokio::test]
    async fn fresh_adr_claims_are_unaffected() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("0001.md"), ADR).expect("write");
        sync(&store, dir.path()).await;

        let out = run(&store, params(dir.path())).await.expect("verify");
        assert!(!out.claims.is_empty());
        assert!(
            out.claims.iter().all(|c| c.disposition == "unaffected"),
            "claims: {:?}",
            out.claims
        );
        let m = out.manifest.expect("manifest");
        assert_eq!(m.by_disposition.get("unprovable"), Some(&0));
    }

    #[tokio::test]
    async fn edited_decision_section_makes_claim_unprovable() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr_path = dir.path().join("0001.md");
        std::fs::write(&adr_path, ADR).expect("write");
        sync(&store, dir.path()).await;

        // Rewrite the Decision section on disk without re-syncing.
        std::fs::write(
            &adr_path,
            ADR.replace(
                "The order service will use event sourcing. State is rebuilt by folding events.",
                "The order service uses a plain CRUD repository backed by Postgres.",
            ),
        )
        .expect("rewrite");

        let out = run(&store, params(dir.path())).await.expect("verify");
        let decision_claim = out
            .claims
            .iter()
            .find(|c| c.subject_type == "decision")
            .expect("decision claim");
        assert_eq!(decision_claim.disposition, "unprovable", "{decision_claim:?}");

        let m = out.manifest.expect("manifest");
        assert!(m.stale_claims.iter().any(|c| c.disposition == "unprovable"));
    }
}
