//! Offline integrity oracle for the ADR index lane.
//!
//! CodeNib's ℱ(G) = ℱ(G) check (docs/DESIGN-claims-and-freshness.md, Step 8):
//! the declared-fact projection produced by incremental `sync_adrs_from_git`
//! must equal the projection produced by a full re-sync from scratch. This tool
//! rebuilds the ADR lane in a throwaway in-memory store and diffs the two
//! projections. It is deliberately off the hot path — run it in CI or manually,
//! never inside a query. The rebuild makes no provider calls (REQ-058).
//!
//! Two lanes are audited: ADR, and symbol (symbols, call-graph edges, file
//! imports, routes). These are the lanes `sync_incremental` maintains
//! incrementally, so they are the ones that can drift from a full rebuild. The
//! commit lane has no incremental path and is reported `not_audited`.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::anchors::normalize_ws;
use crate::domain::entities::{AnchorSource, Claim};
use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::ingest_symbols::{self, IngestSymbolsParams};
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
    /// Glob for source files relative to the repo root, matching the `pattern`
    /// the live symbol index was built with. Default: all supported files. A
    /// live index built with a narrower pattern reads as divergent.
    #[serde(default)]
    pub symbol_glob: Option<String>,
}

fn default_adr_glob() -> String {
    "docs/adr/*.md".to_string()
}

#[derive(Debug, Serialize)]
pub struct LaneIntegrity {
    pub lane: String,
    /// `clean`, `divergent`, or `not_audited`.
    pub status: String,
    /// Why a lane is `not_audited`, when there is something to say beyond
    /// "this lane has no incremental path".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub live_facts: usize,
    pub rebuilt_facts: usize,
    /// Projection keys present live more often than in a full re-sync (stale
    /// rows). A key present in both with different counts reads `key (xN vs xM)`.
    pub only_in_live: Vec<String>,
    /// Projection keys a full re-sync produces more often than the live index
    /// (missing rows), formatted like `only_in_live`.
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

/// Keys whose count in `a` exceeds their count in `b`.
fn surplus(a: &BTreeMap<String, usize>, b: &BTreeMap<String, usize>) -> Vec<String> {
    a.iter()
        .filter_map(|(key, &n)| {
            let m = b.get(key).copied().unwrap_or(0);
            match (n > m, m) {
                (false, _) => None,
                (true, 0) => Some(key.clone()),
                (true, _) => Some(format!("{key} (x{n} vs x{m})")),
            }
        })
        .collect()
}

/// Build the multiset of projection keys for the ADR lane: every open
/// ADR-sourced claim, plus the ADR documents, supersession edges, code links, and
/// temporal edges that ADR sync declares.
async fn adr_projection(
    store: &SqliteStore,
    repo_id: uuid::Uuid,
) -> Result<BTreeMap<String, usize>, Error> {
    let claims = store.open_claims_for_repo(repo_id).await?;
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for fact in store.adr_declared_facts(repo_id).await? {
        *out.entry(fact).or_insert(0) += 1;
    }
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

/// Multiset of the symbol lane's declared facts.
async fn symbol_projection(
    store: &SqliteStore,
    repo_id: uuid::Uuid,
) -> Result<BTreeMap<String, usize>, Error> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for fact in store.symbol_declared_facts(repo_id).await? {
        *out.entry(fact).or_insert(0) += 1;
    }
    Ok(out)
}

/// Diff a live projection against its rebuild into one lane report.
fn lane_report(
    lane: &str,
    live: &BTreeMap<String, usize>,
    rebuilt: &BTreeMap<String, usize>,
) -> LaneIntegrity {
    let only_in_live = surplus(live, rebuilt);
    let only_in_rebuild = surplus(rebuilt, live);
    let clean = only_in_live.is_empty() && only_in_rebuild.is_empty();
    LaneIntegrity {
        lane: lane.to_string(),
        status: if clean { "clean" } else { "divergent" }.to_string(),
        note: None,
        live_facts: live.values().sum(),
        rebuilt_facts: rebuilt.values().sum(),
        only_in_live,
        only_in_rebuild,
    }
}

fn not_audited(lane: &str, note: Option<&str>) -> LaneIntegrity {
    LaneIntegrity {
        lane: lane.to_string(),
        status: "not_audited".to_string(),
        note: note.map(str::to_string),
        live_facts: 0,
        rebuilt_facts: 0,
        only_in_live: vec![],
        only_in_rebuild: vec![],
    }
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

    if let Err(e) = crate::embeddings::without_provider(sync_adrs::run(
        &scratch,
        SyncAdrsFromGitParams {
            repo_path: repo_path_str.to_string(),
            adr_glob: params.adr_glob.clone(),
        },
    ))
    .await
    {
        warnings.push(format!("full re-sync failed: {e}"));
    }
    let rebuilt = adr_projection(&scratch, scratch_repo.id).await?;

    let mut lanes = vec![lane_report("adr", &live, &rebuilt)];

    // The symbol lane is only audited when the live index has one: a repo that
    // was never symbol-indexed would otherwise read as wholly divergent.
    let symbol_indexed = store
        .index_lanes(repo.id)
        .await?
        .iter()
        .any(|l| l.lane == "symbol");
    if symbol_indexed {
        let live_symbols = symbol_projection(store, repo.id).await?;
        let rebuilt_symbols = match crate::embeddings::without_provider(ingest_symbols::run(
            &scratch,
            IngestSymbolsParams {
                repo_path: repo_path_str.to_string(),
                pattern: params.symbol_glob.clone(),
                force: false,
            },
            None,
            None,
        ))
        .await
        {
            Ok(_) => Some(symbol_projection(&scratch, scratch_repo.id).await?),
            Err(e) => {
                warnings.push(format!("symbol full re-index failed: {e}"));
                None
            }
        };
        lanes.push(match rebuilt_symbols {
            Some(rebuilt_symbols) => lane_report("symbol", &live_symbols, &rebuilt_symbols),
            None => not_audited("symbol", Some("full re-index failed; see warnings")),
        });
    } else {
        lanes.push(not_audited("symbol", Some("live index has no symbol lane")));
    }
    lanes.push(not_audited("commit", None));

    let consistent = lanes.iter().all(|l| l.status != "divergent");

    Ok(VerifyIndexIntegrityResult {
        repo_path: repo_path_str.to_string(),
        consistent,
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
                symbol_glob: None,
            },
        )
        .await
        .unwrap();

        assert!(res.consistent, "lanes: {:#?}", res.lanes);
        let adr = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
        assert_eq!(adr.status, "clean");
        assert!(adr.live_facts > 0);
    }

    /// REQ-058: the scratch rebuild is offline. With an embedding provider
    /// configured, it must not open a single connection to it.
    #[tokio::test]
    async fn rebuild_makes_no_embedding_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/adr")).unwrap();
        git2::Repository::init(&repo).unwrap();
        fs::write(
            repo.join("docs/adr/0001-x.md"),
            "# ADR-0001: X\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nServices must use the bus.\n",
        )
        .unwrap();

        let _guard = crate::env_guard();
        let store = store().await;
        let params = || SyncAdrsFromGitParams {
            repo_path: repo.to_string_lossy().to_string(),
            adr_glob: "docs/adr/*.md".to_string(),
        };
        // The live sync runs with no provider; only the oracle's rebuild is under test.
        sync_adrs::run(&store, params()).await.unwrap();

        // A fake Ollama endpoint that counts connections and drops them.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        std::env::set_var("WEAVER_EMBEDDING_PROVIDER", "ollama");
        std::env::set_var("WEAVER_EMBEDDING_URL", &url);

        let res = run(
            &store,
            VerifyIndexIntegrityParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
                symbol_glob: None,
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        std::env::remove_var("WEAVER_EMBEDDING_PROVIDER");
        std::env::remove_var("WEAVER_EMBEDDING_URL");
        server.abort();

        assert!(res.unwrap().consistent);
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "oracle rebuild called the embedding provider"
        );
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
                symbol_glob: None,
            },
        )
        .await
        .unwrap();

        assert!(
            !res.consistent,
            "expected divergence, lanes: {:#?}",
            res.lanes
        );
        let adr_lane = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
        assert_eq!(adr_lane.status, "divergent");
        assert!(!adr_lane.only_in_live.is_empty());
        assert!(!adr_lane.only_in_rebuild.is_empty());
    }
}
