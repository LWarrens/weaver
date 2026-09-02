//! Verify a content-hashed evidence anchor against the current repository state.
//!
//! Verification reads the **working tree** at `repo_path` and records the result
//! keyed by the resolved `HEAD` commit (the cache key). Verifying against an
//! arbitrary historical ref is not supported yet — the common question is "is
//! this still true right now".
//!
//! Never mutates the anchor or the claim; the caller appends the returned
//! `AnchorVerification` via `store.insert_anchor_verification`.

use std::path::Path;

use uuid::Uuid;

use crate::domain::anchors::{content_hash, normalize_ws, token_overlap};
use crate::domain::entities::{
    AnchorSource, AnchorVerification, EditClass, EvidenceAnchor, Freshness, Locator,
};
use crate::storage::SqliteStore;

/// Token-overlap ratio at or above which a non-matching span counts as
/// `affected` (drifted) rather than `deleted`.
const FUZZY_THRESHOLD: f64 = 0.6;

/// The repository state a verification ran against.
#[derive(Debug, Clone)]
pub struct HeadRef {
    pub repo_ref: String,
    pub repo_commit: String,
}

/// Resolve the working tree's `HEAD`. Falls back to a sentinel when the path is
/// not a git repository so verification still runs against the files on disk.
pub fn resolve_head(repo_path: &Path) -> HeadRef {
    let commit_id = git2::Repository::open(repo_path).ok().and_then(|r| {
        let c = r.head().ok()?.peel_to_commit().ok()?;
        Some(c.id().to_string())
    });
    match commit_id {
        Some(sha) => HeadRef {
            repo_ref: "HEAD".to_string(),
            repo_commit: sha,
        },
        None => HeadRef {
            repo_ref: "working-tree".to_string(),
            repo_commit: "working-tree".to_string(),
        },
    }
}

struct Verdict {
    edit_class: EditClass,
    freshness: Freshness,
    relocated: Option<Locator>,
    similarity: Option<f64>,
    detail: String,
}

fn verdict(v: Verdict, anchor: &EvidenceAnchor, head: &HeadRef, now: &str) -> AnchorVerification {
    let observed_hash = match v.freshness {
        Freshness::Fresh => Some(anchor.content_hash.clone()),
        Freshness::Stale => None,
    };
    AnchorVerification {
        id: Uuid::new_v4(),
        anchor_id: anchor.id,
        checked_at: now.to_string(),
        repo_ref: head.repo_ref.clone(),
        repo_commit: head.repo_commit.clone(),
        edit_class: v.edit_class,
        freshness: v.freshness,
        observed_hash,
        relocated_locator: v.relocated,
        similarity: v.similarity,
        detail: Some(v.detail),
    }
}

/// Verify one anchor. Appends nothing — returns the verification for the caller
/// to persist.
pub async fn verify_anchor(
    store: &SqliteStore,
    repo_id: Uuid,
    repo_path: &Path,
    head: &HeadRef,
    anchor: &EvidenceAnchor,
    now: &str,
) -> AnchorVerification {
    let v = match anchor.identity.source_kind {
        AnchorSource::Episode | AnchorSource::Pr => Verdict {
            edit_class: EditClass::Unchanged,
            freshness: Freshness::Fresh,
            relocated: None,
            similarity: None,
            detail: "immutable source".to_string(),
        },
        AnchorSource::Commit => verify_commit(repo_path, &anchor.identity.source_uri),
        AnchorSource::Adr => verify_adr(repo_path, anchor),
        AnchorSource::SourceFile | AnchorSource::Symbol => {
            verify_code(store, repo_id, repo_path, anchor).await
        }
    };
    verdict(v, anchor, head, now)
}

fn verify_commit(repo_path: &Path, sha: &str) -> Verdict {
    let present = git2::Repository::open(repo_path)
        .ok()
        .and_then(|repo| {
            let target = repo.revparse_single(sha).ok()?.id();
            let head = repo.head().ok()?.peel_to_commit().ok()?.id();
            let mut walk = repo.revwalk().ok()?;
            walk.push(head).ok()?;
            Some(walk.filter_map(|o| o.ok()).any(|oid| oid == target))
        })
        .unwrap_or(false);
    if present {
        Verdict {
            edit_class: EditClass::Unchanged,
            freshness: Freshness::Fresh,
            relocated: None,
            similarity: None,
            detail: "commit in history".to_string(),
        }
    } else {
        Verdict {
            edit_class: EditClass::Deleted,
            freshness: Freshness::Stale,
            relocated: None,
            similarity: None,
            detail: format!("commit {sha} not in history of HEAD"),
        }
    }
}

fn verify_adr(repo_path: &Path, anchor: &EvidenceAnchor) -> Verdict {
    let path = repo_path.join(&anchor.identity.source_uri);
    let Ok(source) = std::fs::read_to_string(&path) else {
        return missing("ADR file not found");
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let parsed = crate::domain::adr_parser::parse(&source, stem);
    let section = &anchor.identity.subpath;
    let section_text = match section.as_str() {
        "Decision" => parsed.decision,
        "Consequences" => parsed.consequences,
        "Context" => parsed.context,
        _ => None,
    };
    let Some(section_text) = section_text else {
        return missing(&format!("ADR section '{section}' no longer present"));
    };
    classify_text(&anchor.anchored_text, &section_text, anchor.locator.clone(), None)
}

async fn verify_code(
    store: &SqliteStore,
    repo_id: Uuid,
    repo_path: &Path,
    anchor: &EvidenceAnchor,
) -> Verdict {
    // Resolve the current file + line span.
    let (file_path, span): (String, Option<(u32, u32)>) = match &anchor.locator {
        Locator::SymbolQn { qn } => {
            let bare = qn.rsplit("::").next().unwrap_or(qn);
            match store.resolve_symbol_span(repo_id, bare).await {
                Ok(Some((f, s, e))) => (f, Some((s, e))),
                _ => (anchor.identity.source_uri.clone(), None),
            }
        }
        Locator::Lines { start, end } => {
            (anchor.identity.source_uri.clone(), Some((*start, *end)))
        }
        _ => (anchor.identity.source_uri.clone(), None),
    };

    let Ok(file_text) = std::fs::read_to_string(repo_path.join(&file_path)) else {
        return missing(&format!("file {file_path} not found"));
    };

    let current_span = span.and_then(|(s, e)| slice_lines(&file_text, s, e));
    let current_locator = span.map(|(s, e)| Locator::Lines { start: s, end: e });

    match current_span {
        Some(text) => classify_text(
            &anchor.anchored_text,
            &text,
            anchor.locator.clone(),
            current_locator,
        )
        .or_else_search(&anchor.anchored_text, &file_text),
        None => classify_text(&anchor.anchored_text, &file_text, anchor.locator.clone(), None),
    }
}

/// Slice 1-indexed inclusive `[start, end]` lines.
fn slice_lines(text: &str, start: u32, end: u32) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if start == 0 || end as usize > lines.len() || start > end {
        return None;
    }
    Some(lines[(start as usize - 1)..(end as usize)].join("\n"))
}

/// Minimal contiguous line range whose normalized text contains `needle_norm`.
fn find_span_lines(file_text: &str, needle_norm: &str) -> Option<(u32, u32)> {
    if needle_norm.is_empty() {
        return None;
    }
    let lines: Vec<&str> = file_text.lines().collect();
    for start in 0..lines.len() {
        let mut acc = String::new();
        for (offset, line) in lines[start..].iter().take(80).enumerate() {
            if !acc.is_empty() {
                acc.push(' ');
            }
            acc.push_str(&normalize_ws(line));
            if acc.contains(needle_norm) {
                return Some((start as u32 + 1, (start + offset) as u32 + 1));
            }
            if acc.len() > needle_norm.len() + 512 {
                break;
            }
        }
    }
    None
}

fn classify_text(
    anchored: &str,
    current_span: &str,
    orig_locator: Locator,
    current_locator: Option<Locator>,
) -> Verdict {
    if content_hash(anchored) == content_hash(current_span) {
        return match current_locator {
            Some(loc) if loc != orig_locator => Verdict {
                edit_class: EditClass::Shifted,
                freshness: Freshness::Fresh,
                relocated: Some(loc),
                similarity: None,
                detail: "span moved, content identical".to_string(),
            },
            _ => Verdict {
                edit_class: EditClass::Unchanged,
                freshness: Freshness::Fresh,
                relocated: None,
                similarity: None,
                detail: "exact match".to_string(),
            },
        };
    }
    let anchored_norm = normalize_ws(anchored);
    if normalize_ws(current_span).contains(&anchored_norm) {
        return Verdict {
            edit_class: EditClass::Shifted,
            freshness: Freshness::Fresh,
            relocated: current_locator,
            similarity: None,
            detail: "relocated within span by content".to_string(),
        };
    }
    let sim = token_overlap(anchored, current_span);
    if sim >= FUZZY_THRESHOLD {
        Verdict {
            edit_class: EditClass::Affected,
            freshness: Freshness::Stale,
            relocated: current_locator,
            similarity: Some(sim),
            detail: format!("fuzzy match, token overlap {sim:.2}"),
        }
    } else {
        Verdict {
            edit_class: EditClass::Deleted,
            freshness: Freshness::Stale,
            relocated: None,
            similarity: Some(sim),
            detail: format!("anchored text not found (token overlap {sim:.2})"),
        }
    }
}

impl Verdict {
    /// If this verdict is `deleted`/`stale`, try one more relocation pass over
    /// the whole file before giving up.
    fn or_else_search(self, anchored: &str, file_text: &str) -> Verdict {
        if self.freshness == Freshness::Fresh {
            return self;
        }
        let needle = normalize_ws(anchored);
        if let Some((s, e)) = find_span_lines(file_text, &needle) {
            return Verdict {
                edit_class: EditClass::Shifted,
                freshness: Freshness::Fresh,
                relocated: Some(Locator::Lines { start: s, end: e }),
                similarity: None,
                detail: "relocated across file by content".to_string(),
            };
        }
        self
    }
}

fn missing(detail: &str) -> Verdict {
    Verdict {
        edit_class: EditClass::Deleted,
        freshness: Freshness::Stale,
        relocated: None,
        similarity: None,
        detail: detail.to_string(),
    }
}
