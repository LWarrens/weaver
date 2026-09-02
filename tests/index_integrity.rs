//! ℱ(G) = ℱ(G): the ADR index lane built incrementally over several syncs must
//! match a full re-sync from scratch (docs/DESIGN-claims-and-freshness.md,
//! Step 8). This exercises `verify_index_integrity` end to end.

use std::fs;
use std::sync::Arc;

use weaver::storage::SqliteStore;
use weaver::tools::sync_adrs::{self, SyncAdrsFromGitParams};
use weaver::tools::verify_index_integrity::{self, VerifyIndexIntegrityParams};

async fn store() -> Arc<SqliteStore> {
    let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
    s.run_migrations().await.unwrap();
    Arc::new(s)
}

async fn sync(store: &Arc<SqliteStore>, repo: &std::path::Path) {
    sync_adrs::run(
        store,
        SyncAdrsFromGitParams {
            repo_path: repo.to_string_lossy().to_string(),
            adr_glob: "docs/adr/*.md".to_string(),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn incremental_adr_sync_matches_full_resync() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    let adr_dir = repo.join("docs/adr");
    fs::create_dir_all(&adr_dir).unwrap();
    git2::Repository::init(&repo).unwrap();

    let store = store().await;

    // Sync 1: one ADR.
    fs::write(
        adr_dir.join("0001-a.md"),
        "# ADR-0001: A\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nServices must use the shared bus.\n",
    )
    .unwrap();
    sync(&store, &repo).await;

    // Sync 2: add a second ADR.
    fs::write(
        adr_dir.join("0002-b.md"),
        "# ADR-0002: B\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse Postgres for the catalog.\n\n## Consequences\nMigrations must not run automatically.\n",
    )
    .unwrap();
    sync(&store, &repo).await;

    // Sync 3: supersede the first.
    fs::write(
        adr_dir.join("0001-a.md"),
        "# ADR-0001: A\n\n## Status\nSuperseded by ADR-0002\n\n## Context\nc\n\n## Decision\nServices must use the shared bus.\n",
    )
    .unwrap();
    sync(&store, &repo).await;

    let res = verify_index_integrity::run(
        &store,
        VerifyIndexIntegrityParams {
            repo_path: repo.to_string_lossy().to_string(),
            adr_glob: "docs/adr/*.md".to_string(),
        },
    )
    .await
    .unwrap();

    let adr = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
    assert_eq!(
        adr.status, "clean",
        "incremental != full re-sync:\n only_in_live={:#?}\n only_in_rebuild={:#?}",
        adr.only_in_live, adr.only_in_rebuild
    );
    assert!(res.consistent);
    assert!(adr.live_claims >= 3, "expected >=3 ADR claims, got {}", adr.live_claims);
}
