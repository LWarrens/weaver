use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::entities::TemporalMode;
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileBriefParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Repo-relative file path (e.g. "src/tools/ingest_symbols.rs").
    /// Provide either this or `symbol` — at least one is required.
    #[serde(default)]
    pub file: Option<String>,
    /// Symbol name. Resolved to its containing file; the brief is then for that file.
    /// Provide either this or `file` — at least one is required.
    #[serde(default)]
    pub symbol: Option<String>,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FileBriefResult {
    /// Canonical repo-relative file path this brief describes.
    pub file: String,
    /// Symbols exported/defined by this file: name, kind, line anchor.
    pub exports: Vec<SymbolBrief>,
    /// Files (and their symbols) that call INTO this file.
    pub callers: Vec<CrossRef>,
    /// Files (and their symbols) that this file calls OUT to.
    pub callees: Vec<CrossRef>,
    /// Architectural decisions that govern this file.
    pub decisions: Vec<DecisionBrief>,
    /// Most recent commits that touched this file.
    pub recent_commits: Vec<CommitBrief>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolBrief {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// Compact location anchor: "file:line name"
    pub anchor: String,
}

#[derive(Debug, Serialize)]
pub struct CrossRef {
    pub file: String,
    pub symbols: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DecisionBrief {
    pub adr_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct CommitBrief {
    pub sha7: String,
    pub message: String,
    pub date: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: FileBriefParams,
) -> Result<FileBriefResult, Error> {
    if params.file.is_none() && params.symbol.is_none() {
        return Err(Error::InvalidInput {
            field: "file/symbol",
            reason: "at least one of 'file' or 'symbol' must be provided".to_string(),
        });
    }

    let now = Utc::now().to_rfc3339();
    let valid_at = params.valid_at.as_deref().unwrap_or(&now);

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist or is not accessible: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;
    let mut warnings: Vec<String> = vec![];

    if let Some(w) = crate::tools::freshness::stale_index_warning(&store, repo.id, &repo_path).await {
        warnings.push(w);
    }

    // Resolve file path — either given directly or derived from symbol lookup.
    let file_rel: String = if let Some(ref file) = params.file {
        let joined = repo_path.join(file);
        let normalized = normalize_lexical(&joined);
        if !normalized.starts_with(&repo_path) {
            return Err(Error::InvalidInput {
                field: "file",
                reason: "file path escapes the repository root".to_string(),
            });
        }
        normalized
            .strip_prefix(&repo_path)
            .expect("just checked starts_with")
            .to_string_lossy()
            .replace('\\', "/")
    } else {
        let sym_name = params.symbol.as_deref().unwrap();
        match store.find_symbol_ref_by_name(repo.id, sym_name, valid_at).await? {
            Some(s) => s.file,
            None => {
                return Ok(FileBriefResult {
                    file: String::new(),
                    exports: vec![],
                    callers: vec![],
                    callees: vec![],
                    decisions: vec![],
                    recent_commits: vec![],
                    warnings: vec![format!(
                        "symbol '{}' not found; run ingest_symbols first",
                        sym_name
                    )],
                });
            }
        }
    };

    // Fetch all data in parallel via separate queries.
    let (sym_rows, caller_rows, callee_rows, decision_rows, commit_rows) = tokio::try_join!(
        store.fetch_file_symbols_brief(repo.id, &file_rel, valid_at),
        store.fetch_file_callers_brief(repo.id, &file_rel, valid_at, 30),
        store.fetch_file_callees_brief(repo.id, &file_rel, valid_at, 30),
        store.find_decisions_for_file(repo.id, &file_rel, Some(valid_at), TemporalMode::Event),
        store.fetch_recent_commits_for_file(repo.id, &file_rel, 5),
    )?;

    if sym_rows.is_empty() {
        warnings.push(format!(
            "no symbols indexed for '{}'; run ingest_symbols first",
            file_rel
        ));
    }

    let exports = sym_rows
        .into_iter()
        .map(|(name, kind, line)| SymbolBrief {
            anchor: match line {
                Some(l) => format!("{}:{} {}", file_rel, l, name),
                None => format!("{} {}", file_rel, name),
            },
            name,
            kind,
            line,
        })
        .collect();

    let callers = group_by_file(caller_rows);
    let callees = group_by_file(callee_rows);

    let decisions = decision_rows
        .into_iter()
        .map(|d| DecisionBrief {
            adr_id: d.adr_id,
            title: d.title,
            status: d.status,
        })
        .collect();

    let recent_commits = commit_rows
        .into_iter()
        .map(|(_id, sha, _author, message, date)| CommitBrief {
            sha7: sha.chars().take(7).collect(),
            message: message
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
            date,
        })
        .collect();

    Ok(FileBriefResult {
        file: file_rel,
        exports,
        callers,
        callees,
        decisions,
        recent_commits,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn group_by_file(pairs: Vec<(String, String)>) -> Vec<CrossRef> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (file, sym) in pairs {
        if !map.contains_key(&file) {
            order.push(file.clone());
        }
        map.entry(file).or_default().push(sym);
    }
    order
        .into_iter()
        .map(|file| CrossRef {
            symbols: map.remove(&file).unwrap_or_default(),
            file,
        })
        .collect()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = vec![];
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}
