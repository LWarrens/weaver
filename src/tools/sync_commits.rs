use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::adapters::registry::{extract_symbols_for_extension, has_extractor_for_extension};
use crate::adapters::symbols::Symbol;
use crate::domain::anchors;
use crate::domain::entities::{AnchorIdentity, AnchorSource, Locator};
use crate::embeddings::{pack_f32, provider_from_env};
use crate::error::Error;
use crate::storage::sqlite::TemporalEdge;
use crate::storage::SqliteStore;
use uuid::Uuid;

/// Anchor a decision's open claims to the commit that explicitly references its
/// ADR, so `verify_claims` can cite the implementing commit as evidence
/// (docs/DESIGN-claims-and-freshness.md). Commits are immutable, so these
/// anchors never go stale; INSERT OR IGNORE keeps re-runs idempotent.
async fn anchor_decision_claims_to_commit(
    store: &SqliteStore,
    repo_id: Uuid,
    decision_id: &str,
    sha: &str,
    message: &str,
    now: &str,
    source_time: &str,
) -> Result<(), Error> {
    let Ok(decision_uuid) = Uuid::parse_str(decision_id) else {
        return Ok(());
    };
    let identity = AnchorIdentity {
        source_kind: AnchorSource::Commit,
        source_uri: sha.to_string(),
        subpath: String::new(),
    };
    for claim in store.claims_for_subject("decision", decision_uuid).await? {
        store
            .insert_evidence_anchor(&anchors::build_anchor(
                repo_id,
                claim.id,
                identity.clone(),
                Locator::Chars {
                    start: 0,
                    end: message.chars().count(),
                },
                message,
                None,
                None,
                now,
                Some(source_time.to_string()),
            ))
            .await?;
    }
    Ok(())
}

/// Emit an `evidences` Commit → Decision temporal edge mirroring a
/// `decision_git_links` row, so graph traversal can reach decisions through
/// the commits that implement them. Idempotent across re-runs.
async fn emit_evidences_edge(
    store: &SqliteStore,
    commit_id: &str,
    decision_id: &str,
    confidence: f64,
    now: &str,
) -> Result<bool, Error> {
    let edge = TemporalEdge {
        id: Uuid::new_v4(),
        edge_type: "evidences".to_string(),
        source_id: commit_id.to_string(),
        source_type: "commit".to_string(),
        target_id: decision_id.to_string(),
        target_type: "decision".to_string(),
        valid_from: now.to_string(),
        valid_to: None,
        ingested_at: now.to_string(),
        confidence,
        evidence_refs: vec![],
    };
    store.insert_temporal_edge_if_absent(&edge).await
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncCommitsFromGitParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// Branch to walk (default: HEAD).
    #[serde(default)]
    pub branch: Option<String>,
    /// ISO-8601 timestamp; only include commits at or after this time.
    #[serde(default)]
    pub since: Option<String>,
    /// Maximum number of commits to ingest (default 500).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    500
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

const MAX_WARNINGS: usize = 20;

#[derive(Debug, Serialize)]
pub struct SyncCommitsResult {
    pub commits_ingested: usize,
    pub commits_unchanged: usize,
    pub links_created: usize,
    pub symbols_added: usize,
    pub symbols_removed: usize,
    pub warnings: Vec<String>,
    pub warnings_total: usize,
}

/// Per-file symbol diff extracted from a commit's blob vs its parent's blob.
struct FileSymbolDiff {
    file_path: String,
    added: Vec<Symbol>,
    removed: Vec<(String, String)>, // (name, kind)
}

/// A commit record with pre-computed symbol diffs for all changed source files.
struct RawCommit {
    sha: String,
    source_time: String,
    author: Option<String>,
    message: Option<String>,
    files: Vec<String>,
    symbol_diffs: Vec<FileSymbolDiff>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: SyncCommitsFromGitParams,
) -> Result<SyncCommitsResult, Error> {
    let now = Utc::now().to_rfc3339();

    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!("path does not exist: {}", params.repo_path),
        })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // Collect commits in a blocking thread because git2 types are not Send.
    let branch = params.branch.clone();
    let since_str = params.since.clone();
    let limit = params.limit;
    let now_clone = now.clone();
    let repo_path_for_lane = repo_path.clone();

    let (mut raw_commits, mut warnings) = tokio::task::spawn_blocking(move || {
        collect_commits(&repo_path, branch.as_deref(), since_str.as_deref(), limit, &now_clone)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("spawn_blocking panic: {}", e)))??;

    // Process oldest-first so valid_from/valid_to on symbols are applied in chronological order.
    raw_commits.reverse();

    // Load all current decisions for keyword matching
    let decisions = store.list_all_decisions(repo.id, None).await?;
    let embedding_provider = provider_from_env();

    let mut commits_ingested = 0usize;
    let mut commits_unchanged = 0usize;
    let mut links_created = 0usize;
    let mut symbols_added = 0usize;
    let mut symbols_removed = 0usize;

    for raw in &raw_commits {
        let sha = &raw.sha;
        let source_time = &raw.source_time;
        let author = raw.author.as_deref();
        let message = raw.message.as_deref();
        let files = &raw.files;

        let inserted = store
            .insert_commit(
                repo.id,
                sha,
                author,
                message,
                source_time,
                &now,
            )
            .await?;

        if !inserted {
            commits_unchanged += 1;
            continue;
        }
        commits_ingested += 1;

        // Fetch the commit UUID for linking
        let commit_id = match store.find_commit_id_by_sha(repo.id, sha).await? {
            Some(id) => id,
            None => continue,
        };

        let msg = message.unwrap_or("");

        if let Some(ref provider) = embedding_provider {
            if !msg.is_empty() {
                if let Ok(vec) = provider.embed_chunked(msg, 512).await {
                    if !vec.is_empty() {
                        let blob = pack_f32(&vec);
                        let _ = store.update_commit_embedding(&commit_id, &blob).await;
                    }
                }
            }
        }

        // Phase A: explicit ADR ID reference (confidence 0.95)
        let adr_refs = extract_adr_refs(msg);
        for adr_ref in &adr_refs {
            if let Some(decision) = decisions
                .iter()
                .find(|d| d.adr_id.eq_ignore_ascii_case(adr_ref))
            {
                store
                    .insert_decision_git_link(
                        &decision.id,
                        &commit_id,
                        0.95,
                        source_time,
                        &now,
                        "git_history",
                    )
                    .await?;
                emit_evidences_edge(&store, &commit_id, &decision.id, 0.95, &now).await?;
                anchor_decision_claims_to_commit(
                    &store, repo.id, &decision.id, sha, msg, &now, source_time,
                )
                .await?;
                links_created += 1;
            }
        }

        // Phase B: keyword overlap (confidence 0.6) — only if no explicit ref matched
        if adr_refs.is_empty() {
            let msg_words: std::collections::HashSet<&str> = msg
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 4)
                .collect();

            for decision in &decisions {
                let title = &decision.title;
                let title_words: std::collections::HashSet<&str> = title
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() >= 4)
                    .collect();

                let overlap = msg_words.intersection(&title_words).count();
                if overlap >= 2 {
                    store
                        .insert_decision_git_link(
                            &decision.id,
                            &commit_id,
                            0.6,
                            source_time,
                            &now,
                            "git_history",
                        )
                        .await?;
                    emit_evidences_edge(&store, &commit_id, &decision.id, 0.6, &now).await?;
                    links_created += 1;
                }
            }
        }

        // Record changed files for this commit (commit_files table)
        for fp in files {
            let _ = store
                .insert_commit_file(&commit_id, fp, &now)
                .await;
        }

        // Store symbol-level history derived from tree-sitter blob diffs.
        for diff in &raw.symbol_diffs {
            let file_id = store
                .upsert_file(repo.id, &diff.file_path, &now, source_time)
                .await?;

            for sym in &diff.added {
                let decorators_json = if sym.decorators.is_empty() {
                    None
                } else {
                    serde_json::to_string(&sym.decorators).ok()
                };
                store
                    .insert_symbol(
                        file_id,
                        &sym.name,
                        &sym.kind,
                        sym.start_line as i64,
                        sym.end_line as i64,
                        &now,
                        source_time,
                        sym.signature.as_deref(),
                        sym.return_type.as_deref(),
                        sym.visibility.as_deref(),
                        sym.is_async,
                        sym.complexity,
                        decorators_json.as_deref(),
                    )
                    .await?;
                // Backdate any existing working-tree record whose valid_from is newer.
                store
                    .backdate_symbol_valid_from(file_id, &sym.name, &sym.kind, source_time)
                    .await?;
                symbols_added += 1;
            }

            for (name, kind) in &diff.removed {
                store
                    .close_symbol_by_name(file_id, name, kind, source_time)
                    .await?;
                symbols_removed += 1;
            }
        }
    }

    let warnings_total = warnings.len();
    warnings.truncate(MAX_WARNINGS);

    crate::tools::freshness::record_lane(store, repo.id, &repo_path_for_lane, "commit", "ok").await;

    Ok(SyncCommitsResult {
        commits_ingested,
        commits_unchanged,
        links_created,
        symbols_added,
        symbols_removed,
        warnings,
        warnings_total,
    })
}

/// Walk git commits synchronously. Returns (commits, warnings).
/// Commits are returned newest-first (revwalk order); the caller reverses to oldest-first.
fn collect_commits(
    repo_path: &PathBuf,
    branch: Option<&str>,
    since: Option<&str>,
    limit: usize,
    now: &str,
) -> Result<(Vec<RawCommit>, Vec<String>), Error> {
    let git_repo = git2::Repository::open(repo_path).map_err(|e| Error::InvalidInput {
        field: "repo_path",
        reason: format!("failed to open git repository: {}", e),
    })?;

    let start_oid = if let Some(branch_name) = branch {
        git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .map_err(|e| Error::InvalidInput {
                field: "branch",
                reason: format!("branch '{}' not found: {}", branch_name, e),
            })?
            .get()
            .peel_to_commit()
            .map_err(|e| Error::Other(anyhow::anyhow!(e.to_string())))?
            .id()
    } else {
        git_repo
            .head()
            .map_err(|e| Error::Other(anyhow::anyhow!("failed to get HEAD: {}", e)))?
            .peel_to_commit()
            .map_err(|e| Error::Other(anyhow::anyhow!(e.to_string())))?
            .id()
    };

    let mut revwalk = git_repo
        .revwalk()
        .map_err(|e| Error::Other(anyhow::anyhow!(e.to_string())))?;
    revwalk
        .push(start_oid)
        .map_err(|e| Error::Other(anyhow::anyhow!(e.to_string())))?;
    revwalk
        .set_sorting(git2::Sort::TIME)
        .map_err(|e| Error::Other(anyhow::anyhow!(e.to_string())))?;

    let since_secs: Option<i64> = since
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp());

    let mut commits = Vec::new();
    let mut warnings = Vec::new();

    for oid in revwalk.flatten() {
        if commits.len() >= limit {
            break;
        }
        let commit = match git_repo.find_commit(oid) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("skipped {}: {}", oid, e));
                continue;
            }
        };
        let commit_time = commit.time().seconds();
        if let Some(since) = since_secs {
            if commit_time < since {
                break;
            }
        }
        let sha = oid.to_string();
        let author = commit.author().name().map(str::to_string);
        let message = commit.message().map(|s| s.trim().to_string());
        let source_time = chrono::DateTime::from_timestamp(commit_time, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| now.to_string());

        // Collect changed file paths and compute per-file symbol diffs.
        let mut files_changed: Vec<String> = Vec::new();
        let mut symbol_diffs: Vec<FileSymbolDiff> = Vec::new();

        let current_tree = commit.tree().ok();
        let parent_tree = if commit.parent_count() > 0 {
            commit.parent(0).ok().and_then(|p| p.tree().ok())
        } else {
            None
        };

        if let Some(ref ctree) = current_tree {
            let diff_result = if let Some(ref ptree) = parent_tree {
                git_repo.diff_tree_to_tree(Some(ptree), Some(ctree), None)
            } else {
                git_repo.diff_tree_to_tree(None, Some(ctree), None)
            };

            if let Ok(diff) = diff_result {
                diff.deltas().for_each(|d| {
                    let new_path = d.new_file().path().and_then(|p| p.to_str()).map(str::to_string);
                    let old_path = d.old_file().path().and_then(|p| p.to_str()).map(str::to_string);

                    if let Some(ref fp) = new_path {
                        files_changed.push(fp.clone());

                        let ext = std::path::Path::new(fp)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or_default();
                        if has_extractor_for_extension(ext) {
                            let parent_symbols = old_path
                                .as_deref()
                                .and_then(|op| read_blob_content(&git_repo, parent_tree.as_ref(), op))
                                .and_then(|src| extract_symbols_for_extension(ext, &src).ok())
                                .unwrap_or_default();

                            let current_symbols = read_blob_content(&git_repo, Some(ctree), fp)
                                .and_then(|src| extract_symbols_for_extension(ext, &src).ok())
                                .unwrap_or_default();

                            let sdiff = diff_symbols(parent_symbols, current_symbols);
                            if !sdiff.added.is_empty() || !sdiff.removed.is_empty() {
                                symbol_diffs.push(FileSymbolDiff {
                                    file_path: fp.clone(),
                                    added: sdiff.added,
                                    removed: sdiff.removed,
                                });
                            }
                        }
                    }
                });
            }
        }

        commits.push(RawCommit {
            sha,
            source_time,
            author,
            message,
            files: files_changed,
            symbol_diffs,
        });
    }

    Ok((commits, warnings))
}

/// Read a file blob from a git tree as a UTF-8 string. Returns None for binary files or missing paths.
fn read_blob_content(
    repo: &git2::Repository,
    tree: Option<&git2::Tree>,
    path: &str,
) -> Option<String> {
    let tree = tree?;
    let entry = tree.get_path(std::path::Path::new(path)).ok()?;
    let obj = entry.to_object(repo).ok()?;
    let blob = obj.as_blob()?;
    std::str::from_utf8(blob.content()).ok().map(str::to_string)
}

struct RawSymbolDiff {
    added: Vec<Symbol>,
    removed: Vec<(String, String)>,
}

/// Compute which symbols were added (in `after` but not `before`) and removed (in `before` but not `after`).
/// Identity is (name, kind).
fn diff_symbols(before: Vec<Symbol>, after: Vec<Symbol>) -> RawSymbolDiff {
    use std::collections::HashMap;
    let before_map: HashMap<(&str, &str), &Symbol> = before
        .iter()
        .map(|s| ((s.name.as_str(), s.kind.as_str()), s))
        .collect();
    let after_map: HashMap<(&str, &str), &Symbol> = after
        .iter()
        .map(|s| ((s.name.as_str(), s.kind.as_str()), s))
        .collect();

    let added = after
        .iter()
        .filter(|s| !before_map.contains_key(&(s.name.as_str(), s.kind.as_str())))
        .cloned()
        .collect();

    let removed = before
        .iter()
        .filter(|s| !after_map.contains_key(&(s.name.as_str(), s.kind.as_str())))
        .map(|s| (s.name.clone(), s.kind.clone()))
        .collect();

    RawSymbolDiff { added, removed }
}

/// Extract ADR ID references from commit message text.
/// Matches "ADR-0042", "ADR 42", etc.
fn extract_adr_refs(text: &str) -> Vec<String> {
    let mut refs = vec![];
    let upper = text.to_uppercase();
    let mut pos = 0;
    while let Some(adr_pos) = upper[pos..].find("ADR") {
        let start = pos + adr_pos + 3;
        if start >= upper.len() {
            break;
        }
        let rest = &upper[start..];
        let rest = rest.trim_start_matches(|c: char| c == '-' || c == ' ');
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let n: u32 = digits.parse().unwrap_or(0);
            refs.push(format!("ADR-{:04}", n));
        }
        pos = pos + adr_pos + 3 + 1;
    }
    refs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_adr_refs_parses_formats() {
        assert_eq!(extract_adr_refs("implements ADR-0042"), vec!["ADR-0042"]);
        assert_eq!(extract_adr_refs("fix per ADR 7"), vec!["ADR-0007"]);
        assert_eq!(extract_adr_refs("no references here"), Vec::<String>::new());
        let multi = extract_adr_refs("ADR-0001 and ADR-0002");
        assert_eq!(multi, vec!["ADR-0001", "ADR-0002"]);
    }

    #[tokio::test]
    async fn sync_commits_ingests_and_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
        use std::fs;
        use tempfile::tempdir;

        // Create a real git repo with two commits
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(&repo_path)?;

        let git_repo = git2::Repository::init(&repo_path)?;
        let sig = git2::Signature::now("Test Author", "test@example.com")?;

        // Initial commit
        let file = repo_path.join("README.md");
        fs::write(&file, "initial")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("README.md"))?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = git_repo.find_tree(tree_oid)?;
        git_repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])?;

        // Second commit
        fs::write(&file, "updated")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("README.md"))?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = git_repo.find_tree(tree_oid)?;
        let parent = git_repo.head()?.peel_to_commit()?;
        git_repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Update README",
            &tree,
            &[&parent],
        )?;

        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        // First run — should ingest 2 commits
        let result = run(
            &store,
            SyncCommitsFromGitParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                branch: None,
                since: None,
                limit: 500,
            },
        )
        .await?;
        assert_eq!(result.commits_ingested, 2, "first run ingested");
        assert_eq!(result.commits_unchanged, 0, "first run unchanged");

        // Second run — all unchanged
        let result2 = run(
            &store,
            SyncCommitsFromGitParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                branch: None,
                since: None,
                limit: 500,
            },
        )
        .await?;
        assert_eq!(result2.commits_ingested, 0, "second run ingested");
        assert_eq!(result2.commits_unchanged, 2, "second run unchanged");

        Ok(())
    }

    #[tokio::test]
    async fn sync_commits_extracts_symbol_history() -> Result<(), Box<dyn std::error::Error>> {
        use std::fs;
        use tempfile::tempdir;

        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(&repo_path)?;
        let git_repo = git2::Repository::init(&repo_path)?;
        let sig = git2::Signature::now("Test", "test@test.com")?;

        let commit = |repo: &git2::Repository, msg: &str, sig: &git2::Signature| {
            let mut index = repo.index().unwrap();
            index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.as_ref().map(|c| vec![c]).unwrap_or_default();
            repo.commit(Some("HEAD"), sig, sig, msg, &tree, &parents).unwrap()
        };

        // c1: introduce fn hello
        fs::write(repo_path.join("lib.rs"), "pub fn hello() {}\n")?;
        commit(&git_repo, "add hello", &sig);

        // c2: add fn world, keep hello
        fs::write(repo_path.join("lib.rs"), "pub fn hello() {}\npub fn world() {}\n")?;
        commit(&git_repo, "add world", &sig);

        // c3: remove hello
        fs::write(repo_path.join("lib.rs"), "pub fn world() {}\n")?;
        commit(&git_repo, "remove hello", &sig);

        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        let result = run(
            &store,
            SyncCommitsFromGitParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                branch: None,
                since: None,
                limit: 500,
            },
        )
        .await?;

        assert_eq!(result.commits_ingested, 3);
        // hello added once, world added once = 2 adds; hello removed once = 1 remove
        assert_eq!(result.symbols_added, 2, "hello and world added");
        assert_eq!(result.symbols_removed, 1, "hello removed");

        // The `hello` symbol span should have valid_to set (closed by c3).
        let repo_record = store.upsert_repository(
            repo_path.to_string_lossy().as_ref(),
            None,
        ).await?;
        let spans = store.trace_symbol_spans(repo_record.id, "hello", "0001-01-01T00:00:00Z", "9999-12-31T23:59:59Z").await?;
        assert_eq!(spans.len(), 1, "one span for hello");
        assert!(spans[0].valid_to.is_some(), "hello should be closed");

        // `world` should still be open.
        let world_spans = store.trace_symbol_spans(repo_record.id, "world", "0001-01-01T00:00:00Z", "9999-12-31T23:59:59Z").await?;
        assert_eq!(world_spans.len(), 1);
        assert!(world_spans[0].valid_to.is_none(), "world should still be active");

        Ok(())
    }
}
