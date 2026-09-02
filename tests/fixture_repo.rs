//! Fixture-repository integration test.
//!
//! Builds a real git repository with source files, ADR documents, and commits
//! referencing ADR IDs, then runs the full ingestion pipeline
//! (sync_adrs → ingest_symbols → sync_commits) against a temporary SQLite
//! store and asserts on the resulting knowledge graph.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use weaver::storage::SqliteStore;
use weaver::tools;

/// Commit everything in the work tree with the given message.
fn commit_all(repo: &git2::Repository, msg: &str) -> git2::Oid {
    let sig = git2::Signature::now("Fixture", "fixture@test.invalid").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.as_ref().map(|c| vec![c]).unwrap_or_default();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap()
}

const ADR_0001: &str = r#"# ADR-0001: Use SQLite for storage

## Status

Accepted

Date: 2024-01-10

## Context

We need embedded persistence without a server dependency.

## Decision

We will use SQLite for storage in `src/store.rs`. All writes must go through
the storage layer.

## Consequences

Migrations must be append-only.
"#;

const ADR_0002: &str = r#"# ADR-0002: Route handlers stay thin

## Status

Accepted

Date: 2024-02-20

## Context

Handlers were accumulating business logic.

## Decision

Route handlers in `src/api.rs` must delegate to the storage layer.
"#;

const STORE_RS: &str = r#"pub fn open() -> u32 { 1 }

pub fn write_record(v: u32) -> u32 {
    open() + v
}
"#;

const API_RS: &str = r#"pub fn handle_get() -> u32 { 0 }
"#;

/// Build the fixture repo on disk with two ADRs, two source files, and two
/// commits (the second referencing both ADR IDs).
fn build_fixture(root: &Path) -> git2::Repository {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("0001-sqlite.md"), ADR_0001).unwrap();
    fs::write(root.join("0002-thin-handlers.md"), ADR_0002).unwrap();
    fs::write(root.join("src/store.rs"), STORE_RS).unwrap();

    let repo = git2::Repository::init(root).unwrap();
    commit_all(&repo, "Initial import");

    fs::write(root.join("src/api.rs"), API_RS).unwrap();
    commit_all(&repo, "implement storage and handlers per ADR-0001 and ADR-0002");
    repo
}

async fn test_store(dir: &Path) -> Arc<SqliteStore> {
    let db_url = format!("sqlite:{}?mode=rwc", dir.join("weaver.sqlite").display());
    let store = SqliteStore::connect(&db_url).await.expect("store");
    store.run_migrations().await.expect("migrations");
    Arc::new(store)
}

#[tokio::test]
async fn full_pipeline_builds_knowledge_graph() {
    let td = tempfile::tempdir().expect("tempdir");
    let repo_root = td.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    build_fixture(&repo_root);
    let repo_path = repo_root.to_str().unwrap().to_string();

    let store = test_store(td.path()).await;

    // --- 1. ADR ingestion -------------------------------------------------
    tools::sync_adrs::run(
        &store,
        tools::sync_adrs::SyncAdrsFromGitParams {
            repo_path: repo_path.clone(),
            adr_glob: "*.md".to_string(),
        },
    )
    .await
    .expect("sync_adrs");

    let repo = store.upsert_repository(&repo_path, None).await.expect("repo");
    let decisions = store.list_all_decisions(repo.id, None).await.expect("decisions");
    assert_eq!(decisions.len(), 2, "one decision per ADR");

    let decision_ids: Vec<String> = decisions.iter().map(|d| d.id.clone()).collect();
    let constraints = store
        .find_constraints_for_decisions(&decision_ids)
        .await
        .expect("constraints");
    assert!(
        !constraints.is_empty(),
        "must/shall sentences in ADR text should yield constraints"
    );

    // AdrDocument → Decision and Decision → Constraint edges
    assert_eq!(
        store.count_open_temporal_edges_of_type("defines").await.unwrap(),
        2
    );
    assert_eq!(
        store.count_open_temporal_edges_of_type("imposes").await.unwrap(),
        constraints.len() as i64,
        "one imposes edge per constraint"
    );

    // --- 2. Symbol ingestion ----------------------------------------------
    let sym = tools::ingest_symbols::run(
        &store,
        tools::ingest_symbols::IngestSymbolsParams {
            repo_path: repo_path.clone(),
            pattern: None,
            force: false,
        },
        None,
        None,
    )
    .await
    .expect("ingest_symbols");
    assert!(sym.files_processed >= 2, "both source files processed");
    assert!(!sym.cancelled);
    assert!(
        store.table_count("symbols").await.unwrap() >= 3,
        "open, write_record, handle_get extracted"
    );

    // --- 3. Commit ingestion ----------------------------------------------
    let commits = tools::sync_commits::run(
        &store,
        tools::sync_commits::SyncCommitsFromGitParams {
            repo_path: repo_path.clone(),
            branch: None,
            since: None,
            limit: 500,
        },
    )
    .await
    .expect("sync_commits");
    assert_eq!(commits.commits_ingested, 2);
    assert_eq!(
        commits.links_created, 2,
        "second commit references both ADR IDs"
    );

    // Commit → Decision evidences edges mirror the git links
    assert_eq!(
        store.count_open_temporal_edges_of_type("evidences").await.unwrap(),
        2
    );

    // Re-run is idempotent: no new commits, links, or edges
    let rerun = tools::sync_commits::run(
        &store,
        tools::sync_commits::SyncCommitsFromGitParams {
            repo_path: repo_path.clone(),
            branch: None,
            since: None,
            limit: 500,
        },
    )
    .await
    .expect("sync_commits rerun");
    assert_eq!(rerun.commits_ingested, 0);
    assert_eq!(rerun.commits_unchanged, 2);
    assert_eq!(
        store.count_open_temporal_edges_of_type("evidences").await.unwrap(),
        2,
        "evidences edges must not duplicate on re-run"
    );

    // --- 4. Commit-bridged graph expansion --------------------------------
    // The two decisions share no ADR-typed edge, but are both evidenced by
    // the same commit — the graph leg should bridge them.
    let seed = vec![decision_ids[0].clone()];
    let neighbours = store
        .graph_neighbor_decisions(repo.id, &seed, 1, None, 0.0)
        .await
        .expect("neighbours");
    assert!(
        neighbours.iter().any(|d| d.id == decision_ids[1]),
        "decisions sharing an evidencing commit are one-hop neighbours"
    );

    // --- 5. End-to-end query ----------------------------------------------
    let resp = tools::architecture_query::run(
        &store,
        tools::architecture_query::ArchQueryParams {
            repo_path: repo_path.clone(),
            query: "storage".to_string(),
            valid_at: None,
            top_k: 10,
            graph_depth: 1,
            min_confidence: 0.0,
            include_full_text: false,
            verify: None,
        },
    )
    .await
    .expect("query");
    assert!(
        !resp.decisions.is_empty(),
        "query 'storage' should surface the SQLite decision; warnings: {:?}",
        resp.warnings
    );
}
