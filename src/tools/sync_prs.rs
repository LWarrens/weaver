//! Pull request ingestion and linking (REQ-032).
//!
//! PRs are derived from the merge and squash commits git already carries, so
//! this works offline and on any forge; when a GitHub token is configured the
//! rows are enriched in place with the PR body, author, state, and reviews.
//!
//! Structurally a sibling of [`super::sync_commits`]: same revwalk shape, same
//! 0.95/0.6 decision-link tiers, same anchor and lane bookkeeping.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::adapters::pr_refs::{parse_github_remote, parse_pr_from_commit};
use crate::domain::anchors;
use crate::domain::entities::{AnchorIdentity, AnchorSource, Locator};
use crate::embeddings::{pack_f32, provider_from_env};
use crate::error::Error;
use crate::github::{GitHubClient, PrDetail, PrLookup};
use crate::storage::sqlite::TemporalEdge;
use crate::storage::SqliteStore;
use crate::tools::record_episode::{self, RecordDecisionEpisodeParams};
use crate::tools::sync_commits::extract_adr_refs;

const MAX_WARNINGS: usize = 20;

// ---------------------------------------------------------------------------
// Input / output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncPrsFromGitParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// Branch to walk (default: HEAD).
    #[serde(default)]
    pub branch: Option<String>,
    /// ISO-8601 timestamp; only include pull requests merged at or after this time.
    #[serde(default)]
    pub since: Option<String>,
    /// Maximum number of commits to scan for pull requests (default 200).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Fetch PR bodies and reviews from the GitHub API when a token is
    /// configured and the remote is GitHub. Default true; set false to force
    /// git-only ingestion.
    #[serde(default)]
    pub enrich: Option<bool>,
    /// Feed PR text through episode decision extraction to surface decisions
    /// that were never written up as ADRs. Default true.
    #[serde(default)]
    pub mine_decisions: Option<bool>,
}

fn default_limit() -> usize {
    200
}

#[derive(Debug, Serialize)]
pub struct SyncPrsResult {
    pub prs_ingested: usize,
    pub prs_unchanged: usize,
    pub prs_enriched: usize,
    /// `(#N)` references GitHub confirmed are issues, not pull requests.
    pub prs_not_a_pr: usize,
    pub links_created: usize,
    /// PRs whose text was recorded as an episode for fact extraction.
    pub episodes_recorded: usize,
    /// Architectural facts the LLM extracted from that PR text.
    pub facts_mined: usize,
    /// `github` when API enrichment was active, else `git_only`.
    pub enrichment_mode: String,
    pub warnings: Vec<String>,
    pub warnings_total: usize,
}

/// A pull request identified from git history, before any API enrichment.
struct RawPr {
    pr_ref: String,
    number: Option<u64>,
    /// Identified only from a `(#N)` suffix, so the number may be an issue.
    squash: bool,
    title: Option<String>,
    body: String,
    merge_sha: String,
    author: Option<String>,
    source_time: String,
    files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: SyncPrsFromGitParams,
) -> Result<SyncPrsResult, Error> {
    let now = Utc::now().to_rfc3339();

    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // git2 types are not Send: collect everything git-side in a blocking task.
    let branch = params.branch.clone();
    let since_str = params.since.clone();
    let limit = params.limit;
    let collect_path = repo_path.clone();

    let (mut raw_prs, mut warnings, remote_url) = tokio::task::spawn_blocking(move || {
        collect_prs(&collect_path, branch.as_deref(), since_str.as_deref(), limit)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("spawn_blocking panic: {}", e)))??;

    // Oldest-first, matching sync_commits, so link valid_from ordering is chronological.
    raw_prs.reverse();

    // --- decide whether enrichment is available -----------------------------
    let enrich_requested = params.enrich.unwrap_or(true);
    let github_repo = remote_url.as_deref().and_then(parse_github_remote);
    let client = if enrich_requested { GitHubClient::from_env() } else { None };

    if enrich_requested {
        if remote_url.is_none() {
            warnings.push("no 'origin' remote; PR text limited to merge-commit messages".into());
        } else if github_repo.is_none() {
            warnings.push("remote is not GitHub; PR text limited to merge-commit messages".into());
        } else if client.is_none() {
            warnings.push(
                "no WEAVER_GITHUB_TOKEN/GITHUB_TOKEN; PR text limited to merge-commit messages"
                    .into(),
            );
        }
    }
    let enrichment_active = client.is_some() && github_repo.is_some();
    let enrichment_mode = if enrichment_active { "github" } else { "git_only" };

    let decisions = store.list_all_decisions(repo.id, None).await?;
    let embedding_provider = provider_from_env();
    let mine_decisions = params.mine_decisions.unwrap_or(true);

    let mut prs_ingested = 0usize;
    let mut prs_unchanged = 0usize;
    let mut prs_enriched = 0usize;
    let mut prs_not_a_pr = 0usize;
    let mut links_created = 0usize;
    let mut episodes_recorded = 0usize;
    let mut facts_mined = 0usize;

    for raw in &raw_prs {
        let existing = store.find_pull_request_by_ref(repo.id, &raw.pr_ref).await?;
        let is_new = existing.is_none();

        // --- GitHub lookup: confirms the number is a PR, and enriches it -----
        // Runs before the insert so a `(#N)` that is really an issue is never
        // ingested as a PR. Only git-only records are looked up; a `github` one
        // was confirmed on an earlier run.
        let mut detail: Option<PrDetail> = None;
        let needs_lookup = enrichment_active
            && existing.as_ref().is_none_or(|(_, enrichment)| enrichment == "git");
        if needs_lookup {
            if let (Some(client), Some((owner, name)), Some(number)) =
                (client.as_ref(), github_repo.as_ref(), raw.number)
            {
                match client.lookup_pr(owner, name, number).await {
                    Ok(PrLookup::Pr(d)) => detail = Some(d),
                    Ok(PrLookup::NotAPr) if raw.squash => {
                        prs_not_a_pr += 1;
                        if is_new {
                            warnings.push(format!(
                                "#{number} ends a squash subject but is an issue, not a pull request; not ingested"
                            ));
                            continue;
                        }
                        warnings.push(format!(
                            "#{number} is an issue, not a pull request, but was already ingested from git history; left in place"
                        ));
                    }
                    Ok(PrLookup::NotAPr) | Ok(PrLookup::Unresolved) => warnings.push(format!(
                        "could not confirm #{number} is a pull request on GitHub; left unverified"
                    )),
                    Err(e) => {
                        warnings.push(format!("GitHub enrichment failed for PR {}: {}", raw.pr_ref, e));
                    }
                }
            } else if raw.number.is_none() {
                warnings.push(format!(
                    "PR '{}' has no parseable number; cannot enrich from GitHub",
                    raw.pr_ref
                ));
            }
        }

        if is_new {
            store
                .insert_pull_request(
                    repo.id,
                    &raw.pr_ref,
                    raw.number.map(|n| n as i64),
                    raw.title.as_deref(),
                    Some(&raw.body),
                    Some(&raw.merge_sha),
                    raw.author.as_deref(),
                    Some("merged"),
                    None,
                    "git",
                    &raw.source_time,
                    &now,
                )
                .await?;
            prs_ingested += 1;
        } else {
            prs_unchanged += 1;
        }

        let Some((pr_id, _)) = store.find_pull_request_by_ref(repo.id, &raw.pr_ref).await? else {
            continue;
        };

        if let Some(d) = &detail {
            store
                .update_pull_request_enrichment(
                    &pr_id,
                    d.title.as_deref(),
                    d.body.as_deref(),
                    d.author.as_deref(),
                    d.state.as_deref(),
                    d.url.as_deref(),
                )
                .await?;
            prs_enriched += 1;
        }
        let just_enriched = detail.is_some();

        // Nothing new to link or mine for an untouched PR.
        if !is_new && !just_enriched {
            continue;
        }

        let text = pr_text(raw, detail.as_ref());

        if let Some(ref provider) = embedding_provider {
            if let Ok(vec) = provider.embed_chunked(&text, 512).await {
                if !vec.is_empty() {
                    let blob = pack_f32(&vec);
                    let _ = store.update_pull_request_embedding(&pr_id, &blob).await;
                }
            }
        }

        // --- Phase A: explicit ADR reference (0.95) -------------------------
        let adr_refs = extract_adr_refs(&text);
        for adr_ref in &adr_refs {
            if let Some(decision) = decisions
                .iter()
                .find(|d| d.adr_id.eq_ignore_ascii_case(adr_ref))
            {
                link_decision_to_pr(
                    store,
                    repo.id,
                    &decision.id,
                    &pr_id,
                    &raw.pr_ref,
                    &text,
                    0.95,
                    &raw.source_time,
                    &now,
                )
                .await?;
                links_created += 1;
            }
        }

        // --- Phase B: keyword overlap (0.6), only when no explicit ref -------
        if adr_refs.is_empty() {
            let pr_words: std::collections::HashSet<&str> = text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 4)
                .collect();

            for decision in &decisions {
                let title_words: std::collections::HashSet<&str> = decision
                    .title
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() >= 4)
                    .collect();

                if pr_words.intersection(&title_words).count() >= 2 {
                    link_decision_to_pr(
                        store,
                        repo.id,
                        &decision.id,
                        &pr_id,
                        &raw.pr_ref,
                        &text,
                        0.6,
                        &raw.source_time,
                        &now,
                    )
                    .await?;
                    links_created += 1;
                }
            }
        }

        // --- associate the PR with the commit it merged, and its files ------
        match store.find_commit_id_by_sha(repo.id, &raw.merge_sha).await? {
            Some(commit_id) => {
                let edge = TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "includes".to_string(),
                    source_id: pr_id.clone(),
                    source_type: "pr".to_string(),
                    target_id: commit_id.clone(),
                    target_type: "commit".to_string(),
                    valid_from: raw.source_time.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    confidence: 1.0,
                    evidence_refs: vec![],
                };
                store.insert_temporal_edge_if_absent(&edge).await?;

                for fp in &raw.files {
                    let _ = store.insert_commit_file(&commit_id, fp, &now).await;
                }
            }
            None => warnings.push(format!(
                "merge commit {} for PR {} not ingested; run sync_commits_from_git first",
                &raw.merge_sha[..raw.merge_sha.len().min(8)],
                raw.pr_ref
            )),
        }

        // --- mine PR text for undocumented decisions ------------------------
        //
        // `decisions: None` is deliberate. Passing an explicit EpisodeDecision
        // would record an accepted decision for *every* merged PR; instead the
        // LLM fact-extraction path decides what, if anything, in the PR text is
        // architecturally meaningful. Facts land as temporal edges carrying the
        // model's own confidence — never at `proven` grade.
        if mine_decisions {
            let ep = RecordDecisionEpisodeParams {
                repo_path: repo_path_str.to_string(),
                source: format!("github:pr/{}", raw.pr_ref),
                source_uri: detail.as_ref().and_then(|d| d.url.clone()),
                occurred_at: raw.source_time.clone(),
                content: text.clone(),
                decisions: None,
                dedup_threshold: None,
            };
            match record_episode::run(store, ep).await {
                Ok(resp) => {
                    episodes_recorded += 1;
                    facts_mined += resp.facts_extracted;
                    warnings.extend(resp.warnings);
                }
                Err(e) => warnings.push(format!("decision mining failed for PR {}: {}", raw.pr_ref, e)),
            }
        }
    }

    let warnings_total = warnings.len();
    warnings.truncate(MAX_WARNINGS);

    crate::tools::freshness::record_lane(store, repo.id, &repo_path, "pr", "ok").await;

    Ok(SyncPrsResult {
        prs_ingested,
        prs_unchanged,
        prs_enriched,
        prs_not_a_pr,
        links_created,
        episodes_recorded,
        facts_mined,
        enrichment_mode: enrichment_mode.to_string(),
        warnings,
        warnings_total,
    })
}

/// The text a PR contributes as evidence: title, body, and any review bodies.
fn pr_text(raw: &RawPr, detail: Option<&PrDetail>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let title = detail
        .and_then(|d| d.title.clone())
        .or_else(|| raw.title.clone());
    if let Some(t) = title {
        parts.push(t);
    }
    let body = detail
        .and_then(|d| d.body.clone())
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| raw.body.clone());
    if !body.trim().is_empty() {
        parts.push(body);
    }
    if let Some(d) = detail {
        parts.extend(d.review_bodies.iter().cloned());
    }
    parts.join("\n")
}

/// Record a decision↔PR link, its `evidences` edge, and PR-anchored evidence on
/// the decision's open claims. All three are idempotent across re-runs.
#[allow(clippy::too_many_arguments)]
async fn link_decision_to_pr(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    decision_id: &str,
    pr_id: &str,
    pr_ref: &str,
    text: &str,
    confidence: f64,
    source_time: &str,
    now: &str,
) -> Result<(), Error> {
    store
        .insert_decision_git_link(decision_id, "pr", pr_id, confidence, source_time, now, "git_history")
        .await?;

    let edge = TemporalEdge {
        id: Uuid::new_v4(),
        edge_type: "evidences".to_string(),
        source_id: pr_id.to_string(),
        source_type: "pr".to_string(),
        target_id: decision_id.to_string(),
        target_type: "decision".to_string(),
        valid_from: now.to_string(),
        valid_to: None,
        ingested_at: now.to_string(),
        confidence,
        evidence_refs: vec![],
    };
    store.insert_temporal_edge_if_absent(&edge).await?;

    // A merged PR is immutable, so these anchors never go stale
    // (verify_evidence treats AnchorSource::Pr as an immutable source).
    let Ok(decision_uuid) = Uuid::parse_str(decision_id) else {
        return Ok(());
    };
    let identity = AnchorIdentity {
        source_kind: AnchorSource::Pr,
        source_uri: format!("pr:{pr_ref}"),
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
                    end: text.chars().count(),
                },
                text,
                None,
                None,
                now,
                Some(source_time.to_string()),
            ))
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// git collection
// ---------------------------------------------------------------------------

/// Walk commits and identify pull requests. Returns (prs newest-first, warnings,
/// origin remote URL).
fn collect_prs(
    repo_path: &PathBuf,
    branch: Option<&str>,
    since: Option<&str>,
    limit: usize,
) -> Result<(Vec<RawPr>, Vec<String>, Option<String>), Error> {
    let git_repo = git2::Repository::open(repo_path).map_err(|e| Error::InvalidInput {
        field: "repo_path",
        reason: format!("failed to open git repository: {}", e),
    })?;

    let remote_url = git_repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(str::to_string));

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

    let mut prs = Vec::new();
    let mut warnings = Vec::new();
    let mut scanned = 0usize;

    for oid in revwalk.flatten() {
        if scanned >= limit {
            break;
        }
        scanned += 1;

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

        let message = commit.message().unwrap_or("");
        let subject = message.lines().next().unwrap_or("").trim();
        let body = message
            .split_once('\n')
            .map(|(_, rest)| rest.trim().to_string())
            .unwrap_or_default();

        let Some(pr) = parse_pr_from_commit(subject, &body, commit.parent_count()) else {
            continue;
        };
        let Some(pr_ref) = pr.key() else {
            continue;
        };

        let source_time = chrono::DateTime::from_timestamp(commit_time, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| Utc::now().to_rfc3339());

        // Changed files, relative to the first parent — for a merge commit this
        // is everything the PR brought in.
        let mut files = Vec::new();
        if let Ok(ctree) = commit.tree() {
            let ptree = if commit.parent_count() > 0 {
                commit.parent(0).ok().and_then(|p| p.tree().ok())
            } else {
                None
            };
            if let Ok(diff) = git_repo.diff_tree_to_tree(ptree.as_ref(), Some(&ctree), None) {
                diff.deltas().for_each(|d| {
                    if let Some(p) = d.new_file().path().and_then(|p| p.to_str()) {
                        files.push(p.to_string());
                    }
                });
            }
        }

        prs.push(RawPr {
            pr_ref,
            number: pr.number,
            squash: pr.squash,
            title: pr.title,
            body,
            merge_sha: oid.to_string(),
            author: commit.author().name().map(str::to_string),
            source_time,
            files,
        });
    }

    Ok((prs, warnings, remote_url))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a repo containing one real merge commit (PR #7) and one squash commit (#8).
    fn build_repo(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        fs::create_dir_all(path)?;
        let repo = git2::Repository::init(path)?;
        let sig = git2::Signature::now("Test", "test@test.com")?;

        let commit_all = |repo: &git2::Repository,
                          msg: &str,
                          parents: &[&git2::Commit]|
         -> git2::Oid {
            let mut index = repo.index().unwrap();
            index
                .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, parents)
                .unwrap()
        };

        // base
        fs::write(path.join("a.txt"), "a")?;
        let base_oid = commit_all(&repo, "Initial commit", &[]);
        let base = repo.find_commit(base_oid)?;

        // feature branch commit
        repo.branch("feat", &base, false)?;
        repo.set_head("refs/heads/feat")?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        fs::write(path.join("b.txt"), "b")?;
        let feat_oid = commit_all(&repo, "work on feature", &[&base]);
        let feat = repo.find_commit(feat_oid)?;

        // merge it back with a GitHub-style merge subject
        repo.set_head("refs/heads/master")?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        let base = repo.find_commit(base_oid)?;
        commit_all(
            &repo,
            "Merge pull request #7 from acme/feat\n\nAdd the feature",
            &[&base, &feat],
        );

        // a squash-merge style commit
        fs::write(path.join("c.txt"), "c")?;
        let head = repo.head()?.peel_to_commit()?;
        commit_all(&repo, "Add response cache (#8)", &[&head]);

        Ok(())
    }

    async fn store_for(dir: &std::path::Path) -> Result<Arc<SqliteStore>, Box<dyn std::error::Error>> {
        let db_url = format!("sqlite:{}?mode=rwc", dir.join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;
        Ok(store)
    }

    fn params(repo_path: &std::path::Path) -> SyncPrsFromGitParams {
        SyncPrsFromGitParams {
            repo_path: repo_path.to_string_lossy().to_string(),
            branch: None,
            since: None,
            limit: 200,
            enrich: Some(false),
            mine_decisions: Some(false),
        }
    }

    #[tokio::test]
    async fn ingests_prs_and_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        build_repo(&repo_path)?;
        let store = store_for(td.path()).await?;

        let first = run(&store, params(&repo_path)).await?;
        assert_eq!(first.prs_ingested, 2, "merge PR #7 and squash PR #8");
        assert_eq!(first.prs_unchanged, 0);

        let second = run(&store, params(&repo_path)).await?;
        assert_eq!(second.prs_ingested, 0, "re-run ingests nothing");
        assert_eq!(second.prs_unchanged, 2);

        Ok(())
    }

    // --- GitHub verification of `(#N)` references ---------------------------

    const PR_7: &str = r#"{"number":7,"title":"Add the feature","body":"b","state":"closed","merged_at":"2026-01-01T00:00:00Z","html_url":"https://github.com/acme/weaver/pull/7","user":{"login":"octo"}}"#;
    const PR_8: &str = r#"{"number":8,"title":"Add response cache","body":"b","state":"closed","merged_at":"2026-01-02T00:00:00Z","html_url":"https://github.com/acme/weaver/pull/8","user":{"login":"octo"}}"#;
    const ISSUE_8: &str = r#"{"number":8,"title":"cache request"}"#;

    /// A local GitHub API stub: exact-path routes, 404 for anything else.
    async fn github_stub(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, s, b)| (*s, *b))
                    .unwrap_or((404, r#"{"message":"Not Found"}"#));
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        url
    }

    /// The fixture repo (PR #7 by merge commit, #8 by squash) with a GitHub origin.
    fn github_repo(td: &tempfile::TempDir) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        let repo_path = td.path().join("repo");
        build_repo(&repo_path)?;
        git2::Repository::open(&repo_path)?.remote("origin", "https://github.com/acme/weaver")?;
        Ok(repo_path)
    }

    fn with_github(base: &str) {
        std::env::set_var("WEAVER_GITHUB_TOKEN", "t0ken");
        std::env::set_var("WEAVER_GITHUB_API_BASE", base);
    }

    fn without_github() {
        std::env::remove_var("WEAVER_GITHUB_TOKEN");
        std::env::remove_var("WEAVER_GITHUB_API_BASE");
    }

    fn enriching(repo_path: &std::path::Path) -> SyncPrsFromGitParams {
        let mut p = params(repo_path);
        p.enrich = Some(true);
        p
    }

    #[tokio::test]
    async fn squash_reference_that_is_an_issue_is_not_ingested() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        let td = tempdir()?;
        let repo_path = github_repo(&td)?;
        let store = store_for(td.path()).await?;
        let base = github_stub(vec![
            ("/repos/acme/weaver/pulls/7", 200, PR_7),
            ("/repos/acme/weaver/pulls/8", 404, "{}"),
            ("/repos/acme/weaver/issues/8", 200, ISSUE_8),
        ])
        .await;
        with_github(&base);
        let result = run(&store, enriching(&repo_path)).await;
        without_github();
        let result = result?;

        assert_eq!(result.prs_ingested, 1, "only the real PR: {:?}", result.warnings);
        assert_eq!(result.prs_not_a_pr, 1);
        let repo = store.upsert_repository(&dunce::canonicalize(&repo_path)?.to_string_lossy(), None).await?;
        assert!(store.find_pull_request_by_ref(repo.id, "8").await?.is_none());
        let (_, enrichment) = store.find_pull_request_by_ref(repo.id, "7").await?.unwrap();
        assert_eq!(enrichment, "github");
        assert!(result.warnings.iter().any(|w| w.contains("#8") && w.contains("issue")));
        Ok(())
    }

    #[tokio::test]
    async fn squash_reference_confirmed_as_a_pr_is_ingested_and_enriched() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        let td = tempdir()?;
        let repo_path = github_repo(&td)?;
        let store = store_for(td.path()).await?;
        let base = github_stub(vec![
            ("/repos/acme/weaver/pulls/7", 200, PR_7),
            ("/repos/acme/weaver/pulls/8", 200, PR_8),
        ])
        .await;
        with_github(&base);
        let result = run(&store, enriching(&repo_path)).await;
        without_github();
        let result = result?;

        assert_eq!((result.prs_ingested, result.prs_enriched, result.prs_not_a_pr), (2, 2, 0));
        Ok(())
    }

    /// GitHub answers 404 for a repo the token cannot see, so a 404 on both the
    /// pull and the issue proves nothing: keep the PR, unverified.
    #[tokio::test]
    async fn unresolved_404_leaves_a_squash_pr_unverified() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        let td = tempdir()?;
        let repo_path = github_repo(&td)?;
        let store = store_for(td.path()).await?;
        let base = github_stub(vec![("/repos/acme/weaver/pulls/7", 200, PR_7)]).await;
        with_github(&base);
        let result = run(&store, enriching(&repo_path)).await;
        without_github();
        let result = result?;

        assert_eq!((result.prs_ingested, result.prs_not_a_pr), (2, 0));
        let repo = store.upsert_repository(&dunce::canonicalize(&repo_path)?.to_string_lossy(), None).await?;
        let (_, enrichment) = store.find_pull_request_by_ref(repo.id, "8").await?.unwrap();
        assert_eq!(enrichment, "git", "stays a git-only, unverified record");
        assert!(result.warnings.iter().any(|w| w.contains("could not confirm #8")));
        Ok(())
    }

    /// A merge-commit subject names a pull request outright; the API disagreeing
    /// is a warning, never a reason to drop it.
    #[tokio::test]
    async fn merge_commit_pr_is_never_dropped() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        let td = tempdir()?;
        let repo_path = github_repo(&td)?;
        let store = store_for(td.path()).await?;
        let base = github_stub(vec![
            ("/repos/acme/weaver/pulls/7", 404, "{}"),
            ("/repos/acme/weaver/issues/7", 200, r#"{"number":7}"#),
            ("/repos/acme/weaver/pulls/8", 200, PR_8),
        ])
        .await;
        with_github(&base);
        let result = run(&store, enriching(&repo_path)).await;
        without_github();
        let result = result?;

        assert_eq!((result.prs_ingested, result.prs_not_a_pr), (2, 0));
        assert!(result.warnings.iter().any(|w| w.contains("could not confirm #7")));
        Ok(())
    }

    /// A row ingested before verification existed is reported, not silently deleted.
    #[tokio::test]
    async fn already_ingested_issue_reference_is_reported_and_left_in_place() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        let td = tempdir()?;
        let repo_path = github_repo(&td)?;
        let store = store_for(td.path()).await?;
        run(&store, params(&repo_path)).await?; // git-only first: both ingested

        let base = github_stub(vec![
            ("/repos/acme/weaver/pulls/7", 200, PR_7),
            ("/repos/acme/weaver/pulls/8", 404, "{}"),
            ("/repos/acme/weaver/issues/8", 200, ISSUE_8),
        ])
        .await;
        with_github(&base);
        let result = run(&store, enriching(&repo_path)).await;
        without_github();
        let result = result?;

        assert_eq!(result.prs_not_a_pr, 1);
        let repo = store.upsert_repository(&dunce::canonicalize(&repo_path)?.to_string_lossy(), None).await?;
        assert!(store.find_pull_request_by_ref(repo.id, "8").await?.is_some());
        assert!(result.warnings.iter().any(|w| w.contains("already ingested")));
        Ok(())
    }

    #[tokio::test]
    async fn degrades_to_git_only_without_token() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        std::env::remove_var("WEAVER_GITHUB_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");

        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        build_repo(&repo_path)?;
        let store = store_for(td.path()).await?;

        let mut p = params(&repo_path);
        p.enrich = Some(true); // ask for enrichment; no token and no remote exist
        let result = run(&store, p).await?;

        assert_eq!(result.enrichment_mode, "git_only");
        assert_eq!(result.prs_ingested, 2, "PRs still ingested without enrichment");
        assert!(
            result.warnings.iter().any(|w| w.contains("origin")),
            "expected a degradation warning, got {:?}",
            result.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn mines_facts_from_pr_text_once_per_pr() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = crate::env_guard();
        std::env::set_var("WEAVER_LLM_PROVIDER", "mock");
        std::env::set_var(
            "WEAVER_LLM_RESPONSE",
            r#"[{"subject":"orders","relation":"must_use","object":"response cache","confidence":0.7}]"#,
        );

        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        build_repo(&repo_path)?;
        let store = store_for(td.path()).await?;

        let mut p = params(&repo_path);
        p.mine_decisions = Some(true);
        let first = run(&store, p).await?;
        assert_eq!(first.episodes_recorded, 2, "both PRs recorded as episodes");
        assert_eq!(first.facts_mined, 2, "one fact extracted per PR");

        // Mining is gated on first sight, so a re-sync must not re-invoke the LLM.
        let mut p2 = params(&repo_path);
        p2.mine_decisions = Some(true);
        let second = run(&store, p2).await?;
        assert_eq!(second.episodes_recorded, 0, "re-sync mines nothing");
        assert_eq!(second.facts_mined, 0);

        std::env::remove_var("WEAVER_LLM_PROVIDER");
        std::env::remove_var("WEAVER_LLM_RESPONSE");
        Ok(())
    }

    #[tokio::test]
    async fn links_pr_to_decision_by_explicit_adr_reference()
    -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("docs/adr"))?;
        fs::write(
            repo_path.join("docs/adr/0001-use-a-cache.md"),
            "# ADR-0001: Use a response cache\n\n## Status\n\nAccepted\n\n## Context\n\nLatency.\n\n## Decision\n\nWe will use a response cache.\n\n## Consequences\n\nMore memory.\n",
        )?;

        let repo = git2::Repository::init(&repo_path)?;
        let sig = git2::Signature::now("Test", "test@test.com")?;
        let mut index = repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let base_oid = repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])?;
        let base = repo.find_commit(base_oid)?;
        // A squash-merge PR whose body cites the ADR.
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Wire up the cache (#12)\n\nImplements ADR-0001.",
            &tree,
            &[&base],
        )?;

        let store = store_for(td.path()).await?;
        crate::tools::sync_adrs::run(
            &store,
            crate::tools::sync_adrs::SyncAdrsFromGitParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                adr_glob: "docs/adr/*.md".to_string(),
            },
        )
        .await?;

        let result = run(&store, params(&repo_path)).await?;
        assert_eq!(result.prs_ingested, 1);
        assert_eq!(result.links_created, 1, "explicit ADR reference links the PR");

        let repo_rec = store
            .upsert_repository(repo_path.to_string_lossy().as_ref(), None)
            .await?;
        let (count, confidence): (i64, f64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(confidence), 0.0) FROM decision_git_links WHERE ref_type = 'pr'",
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(count, 1, "one pr-typed link row");
        assert!((confidence - 0.95).abs() < f64::EPSILON, "explicit tier");
        let _ = repo_rec;

        Ok(())
    }
}
