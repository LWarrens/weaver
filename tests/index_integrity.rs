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
            symbol_glob: None,
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
    assert!(
        adr.live_facts >= 3,
        "expected >=3 ADR facts, got {}",
        adr.live_facts
    );
}

const ADR_V1: &str = "# ADR-0001: Bus\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nServices must use the bus in `src/bus.rs`.\n";

async fn oracle(
    store: &Arc<SqliteStore>,
    repo: &std::path::Path,
) -> verify_index_integrity::VerifyIndexIntegrityResult {
    verify_index_integrity::run(
        store,
        VerifyIndexIntegrityParams {
            repo_path: repo.to_string_lossy().to_string(),
            adr_glob: "docs/adr/*.md".to_string(),
            symbol_glob: None,
        },
    )
    .await
    .unwrap()
}

/// A status-only edit changes no claim, so a claims-only projection cannot see
/// that the live index is stale. The ADR document's status is a declared fact.
#[tokio::test]
async fn detects_status_change_without_resync() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    fs::create_dir_all(repo.join("docs/adr")).unwrap();
    git2::Repository::init(&repo).unwrap();
    let store = store().await;

    fs::write(repo.join("docs/adr/0001-bus.md"), ADR_V1).unwrap();
    sync(&store, &repo).await;
    fs::write(
        repo.join("docs/adr/0001-bus.md"),
        ADR_V1.replace("Accepted", "Deprecated"),
    )
    .unwrap();

    let res = oracle(&store, &repo).await;
    let adr = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
    assert_eq!(adr.status, "divergent", "lane: {adr:#?}");
    assert!(adr
        .only_in_live
        .iter()
        .any(|k| k.starts_with("adr|ADR-0001|accepted")));
    assert!(adr
        .only_in_rebuild
        .iter()
        .any(|k| k.starts_with("adr|ADR-0001|deprecated")));
}

/// Editing an ADR and re-syncing closes the old decision. Its code links and
/// temporal edges must close with it, or the live index keeps open rows a full
/// re-sync would never produce.
#[tokio::test]
async fn adr_edit_and_resync_leaves_no_stale_open_rows() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    fs::create_dir_all(repo.join("docs/adr")).unwrap();
    git2::Repository::init(&repo).unwrap();
    let store = store().await;

    fs::write(repo.join("docs/adr/0001-bus.md"), ADR_V1).unwrap();
    sync(&store, &repo).await;
    fs::write(
        repo.join("docs/adr/0001-bus.md"),
        ADR_V1.replace("`src/bus.rs`", "`src/bus.rs` and `src/queue.rs`"),
    )
    .unwrap();
    sync(&store, &repo).await;

    let res = oracle(&store, &repo).await;
    let adr = res.lanes.iter().find(|l| l.lane == "adr").unwrap();
    assert_eq!(
        adr.status, "clean",
        "only_in_live={:#?}\n only_in_rebuild={:#?}",
        adr.only_in_live, adr.only_in_rebuild
    );
}

// ---------------------------------------------------------------------------
// Symbol lane
// ---------------------------------------------------------------------------

async fn ingest(store: &Arc<SqliteStore>, repo: &std::path::Path) {
    weaver::tools::ingest_symbols::run(
        store,
        weaver::tools::ingest_symbols::IngestSymbolsParams {
            repo_path: repo.to_string_lossy().to_string(),
            pattern: None,
            force: false,
        },
        None,
        None,
    )
    .await
    .unwrap();
}

fn lane<'a>(
    res: &'a verify_index_integrity::VerifyIndexIntegrityResult,
    name: &str,
) -> &'a verify_index_integrity::LaneIntegrity {
    res.lanes.iter().find(|l| l.lane == name).unwrap()
}

fn symbol_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    git2::Repository::init(&repo).unwrap();
    (td, repo)
}

/// The symbol lane built over several incremental ingests (a callee renamed
/// without touching its unchanged caller, a file added, a file deleted) must
/// equal a full re-index from scratch.
#[tokio::test]
async fn incremental_symbol_ingest_matches_full_reindex() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(repo.join("src/util.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    fs::write(
        repo.join("src/app.rs"),
        "pub fn run() -> i32 { helper() }\n",
    )
    .unwrap();
    ingest(&store, &repo).await;

    // Rename the callee; the caller's file is unchanged and is skipped.
    fs::write(repo.join("src/util.rs"), "pub fn assist() -> i32 { 1 }\n").unwrap();
    ingest(&store, &repo).await;

    // Add a file, then delete one.
    fs::write(repo.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    ingest(&store, &repo).await;
    fs::remove_file(repo.join("src/extra.rs")).unwrap();
    ingest(&store, &repo).await;

    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(
        sym.status, "clean",
        "incremental != full re-index:\n only_in_live={:#?}\n only_in_rebuild={:#?}",
        sym.only_in_live, sym.only_in_rebuild
    );
    assert!(sym.live_facts >= 2, "expected symbol facts, got {sym:#?}");
}

/// A source edit that was never re-ingested leaves the live index stale.
#[tokio::test]
async fn detects_unindexed_source_edit() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(repo.join("src/util.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    ingest(&store, &repo).await;
    fs::write(repo.join("src/util.rs"), "pub fn renamed() -> i32 { 1 }\n").unwrap();

    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(sym.status, "divergent", "lane: {sym:#?}");
    assert!(!res.consistent);
    assert!(sym
        .only_in_live
        .iter()
        .any(|k| k.starts_with("symbol|src/util.rs|function_item|helper")));
    assert!(sym
        .only_in_rebuild
        .iter()
        .any(|k| k.starts_with("symbol|src/util.rs|function_item|renamed")));
}

/// A repo that was never symbol-indexed must not read as wholly divergent.
#[tokio::test]
async fn unindexed_symbol_lane_is_not_audited() {
    let (_td, repo) = symbol_repo();
    let store = store().await;
    fs::write(repo.join("src/util.rs"), "pub fn helper() {}\n").unwrap();

    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(sym.status, "not_audited");
    assert!(sym.note.is_some());
    assert!(res.consistent);
}

/// Deleting a file and later restoring it byte-for-byte must re-index it: the
/// content hash of a retired file is cleared, or the restored file is skipped
/// as "unchanged" and its symbols stay closed.
#[tokio::test]
async fn restored_file_is_reindexed() {
    let (_td, repo) = symbol_repo();
    let store = store().await;
    let body = "pub fn helper() -> i32 { 1 }\n";

    fs::write(repo.join("src/util.rs"), body).unwrap();
    ingest(&store, &repo).await;
    fs::remove_file(repo.join("src/util.rs")).unwrap();
    ingest(&store, &repo).await;
    fs::write(repo.join("src/util.rs"), body).unwrap();
    ingest(&store, &repo).await;

    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(
        sym.status, "clean",
        "only_in_live={:#?}\n only_in_rebuild={:#?}",
        sym.only_in_live, sym.only_in_rebuild
    );
    assert!(sym.live_facts >= 1, "restored symbol missing: {sym:#?}");
}

/// Removing a file's last route must close it; a file with no routes used to be
/// skipped before its stale routes were closed.
#[tokio::test]
async fn removed_last_route_is_closed() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    git2::Repository::init(&repo).unwrap();
    let store = store().await;

    fs::write(
        repo.join("src/server.js"),
        "const app = require('express')();\nfunction list(req, res) {}\napp.get('/items', list);\n",
    )
    .unwrap();
    ingest(&store, &repo).await;
    fs::write(
        repo.join("src/server.js"),
        "const app = require('express')();\nfunction list(req, res) {}\n",
    )
    .unwrap();
    ingest(&store, &repo).await;

    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(
        sym.status, "clean",
        "only_in_live={:#?}\n only_in_rebuild={:#?}",
        sym.only_in_live, sym.only_in_rebuild
    );
}

/// Routes in Ruby, PHP, and C# files reach the `routes` table through the normal
/// ingest path, not just the extractor unit tests.
#[tokio::test]
async fn ingest_persists_routes_for_ruby_php_and_csharp() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::create_dir_all(repo.join("routes")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    git2::Repository::init(&repo).unwrap();
    fs::write(
        repo.join("config/routes.rb"),
        "Rails.application.routes.draw do\n  get '/about', to: 'pages#about'\nend\n",
    )
    .unwrap();
    fs::write(
        repo.join("routes/web.php"),
        "<?php\nRoute::get('/users', [UserController::class, 'index']);\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/Program.cs"),
        "class P { void M() { app.MapGet(\"/ping\", Ping); } }\n",
    )
    .unwrap();

    let store = store().await;
    ingest(&store, &repo).await;
    let repo_row = store
        .upsert_repository(
            &dunce::canonicalize(&repo).unwrap().to_string_lossy(),
            None,
        )
        .await
        .unwrap();
    let facts = store.symbol_declared_facts(repo_row.id).await.unwrap();
    for expected in [
        "route|GET|/about|rails|config/routes.rb:2|",
        "route|GET|/users|laravel|routes/web.php:2|",
        "route|GET|/ping|aspnet|src/Program.cs:1|",
    ] {
        assert!(
            facts.iter().any(|f| f.starts_with(expected)),
            "missing {expected}; route facts: {:#?}",
            facts.iter().filter(|f| f.starts_with("route|")).collect::<Vec<_>>()
        );
    }
}

async fn open_contains(store: &Arc<SqliteStore>, repo: &std::path::Path) -> Vec<String> {
    let repo_row = store
        .upsert_repository(&dunce::canonicalize(repo).unwrap().to_string_lossy(), None)
        .await
        .unwrap();
    let mut facts: Vec<String> = store
        .symbol_declared_facts(repo_row.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|f| f.starts_with("edge|") && f.contains("|contains|"))
        .collect();
    facts.sort();
    facts
}

/// Re-ingesting a changed file re-emits its `contains` edges. The previous ones
/// must close, or the live index accumulates duplicates a full re-index never has.
#[tokio::test]
async fn reingesting_a_file_does_not_duplicate_contains_edges() {
    let (_td, repo) = symbol_repo();
    let store = store().await;
    let outer = "pub struct Widget;\nimpl Widget {\n    pub fn draw(&self) {}\n    pub fn hide(&self) {}\n}\n";
    fs::write(repo.join("src/widget.rs"), outer).unwrap();
    ingest(&store, &repo).await;
    let once = open_contains(&store, &repo).await;
    assert!(!once.is_empty(), "fixture must produce contains edges");

    // An unrelated edit to the same file forces it to be re-ingested.
    fs::write(repo.join("src/widget.rs"), format!("{outer}// touched\n")).unwrap();
    ingest(&store, &repo).await;

    assert_eq!(open_contains(&store, &repo).await, once, "contains edges must not accumulate");
    let res = oracle(&store, &repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(sym.status, "clean", "{:#?}", sym);
}

/// A container with an outgoing edge of its own (here a Python base class) must keep its
/// `contains` edges. The per-source closer used to close them right after they
/// were emitted, so receiver-type resolution could never find its methods.
#[tokio::test]
async fn container_with_an_outgoing_edge_keeps_its_contains_edges() {
    let (_td, repo) = symbol_repo();
    let store = store().await;
    fs::write(
        repo.join("src/shapes.py"),
        "class Base:\n    pass\n\nclass Circle(Base):\n    def area(self):\n        return 1\n\n    def draw(self):\n        return 2\n",
    )
    .unwrap();
    ingest(&store, &repo).await;

    let contains = open_contains(&store, &repo).await;
    assert!(
        contains.iter().any(|f| f.contains("|Circle|contains|") && f.contains("|area|")),
        "Circle must contain area(); open contains edges: {contains:#?}"
    );
}

// ---------------------------------------------------------------------------
// Name-dependent edge resolution
// ---------------------------------------------------------------------------

async fn assert_symbol_lane_clean(store: &Arc<SqliteStore>, repo: &std::path::Path) {
    let res = oracle(store, repo).await;
    let sym = lane(&res, "symbol");
    assert_eq!(
        sym.status, "clean",
        "incremental != full re-index:\n only_in_live={:#?}\n only_in_rebuild={:#?}",
        sym.only_in_live, sym.only_in_rebuild
    );
}

/// A caller resolved by unique name must go back to unresolved when another file
/// adds a second symbol with that name, and again resolve when it is removed.
#[tokio::test]
async fn unique_name_edge_follows_name_becoming_ambiguous_and_unique_again() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(repo.join("src/util.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    fs::write(repo.join("src/app.rs"), "pub fn run() -> i32 { helper() }\n").unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;

    // app.rs is unchanged; `helper` is now ambiguous.
    fs::write(repo.join("src/other.rs"), "pub fn helper() -> i32 { 2 }\n").unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;

    // ...and unique again.
    fs::remove_file(repo.join("src/other.rs")).unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;
}

/// An edge left unresolved because its target did not exist yet resolves when a
/// later run adds the target, without touching the caller's file.
#[tokio::test]
async fn unresolved_edge_resolves_when_target_appears_later() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(repo.join("src/app.rs"), "pub fn run() -> i32 { helper() }\n").unwrap();
    ingest(&store, &repo).await;

    fs::write(repo.join("src/util.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;
}

/// Removing a function from a re-ingested file retires its symbol; the edges it
/// had must not linger open with a retired source.
#[tokio::test]
async fn removed_function_takes_its_outgoing_edges_with_it() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(
        repo.join("src/app.rs"),
        "pub fn run() -> i32 { helper() }\npub fn stay() {}\n",
    )
    .unwrap();
    fs::write(repo.join("src/util.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    ingest(&store, &repo).await;

    fs::write(repo.join("src/app.rs"), "pub fn stay() {}\n").unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;
}

/// A file that becomes ignored is retired, and other files' imports of it must
/// stop resolving to it, as they would not on a from-scratch index.
#[tokio::test]
async fn imports_of_a_newly_ignored_file_go_unresolved() {
    let (_td, repo) = symbol_repo();
    let store = store().await;

    fs::write(repo.join("lib.js"), "export function helper() { return 1; }\n").unwrap();
    fs::write(
        repo.join("src/app.js"),
        "import { helper } from '../lib.js';\nexport function run() { return helper(); }\n",
    )
    .unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;

    fs::write(repo.join(".archignore"), "/lib.js\n").unwrap();
    ingest(&store, &repo).await;
    assert_symbol_lane_clean(&store, &repo).await;
}
