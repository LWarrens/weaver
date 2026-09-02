//! Build content-hashed evidence anchors from captured source spans.
//!
//! Pure. No I/O. The hash is taken over the *normalized span content*, not the
//! whole file, so a sibling edit in the same file is not a false alarm.

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::entities::{AnchorIdentity, EvidenceAnchor, Locator};

/// Whitespace-normalize a span: collapse every run of whitespace to a single
/// space and trim. Case is preserved — code is case-sensitive.
pub fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// sha256 hex of the whitespace-normalized text.
pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(normalize_ws(text).as_bytes());
    hex(h.finalize().as_slice())
}

/// Jaccard overlap of whitespace tokens, 0.0..=1.0. Used for the fuzzy
/// relocation fallback during verification.
pub fn token_overlap(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<&str> = a.split_whitespace().collect();
    let sb: HashSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Number of context lines captured on each side of an anchored span.
pub const CONTEXT_LINES: usize = 8;

/// Slice `±CONTEXT_LINES` around `[start_line, end_line]` (1-indexed, inclusive)
/// from `file_text` and hash it. Returns `None` when the range is out of bounds.
pub fn context_hash_for_lines(file_text: &str, start_line: u32, end_line: u32) -> Option<String> {
    let lines: Vec<&str> = file_text.lines().collect();
    if start_line == 0 || end_line as usize > lines.len() || start_line > end_line {
        return None;
    }
    let lo = (start_line as usize).saturating_sub(1 + CONTEXT_LINES);
    let hi = (end_line as usize + CONTEXT_LINES).min(lines.len());
    Some(content_hash(&lines[lo..hi].join("\n")))
}

/// Construct an anchor from a captured span. `context` is the optional enclosing
/// window text (already sliced by the caller).
#[allow(clippy::too_many_arguments)]
pub fn build_anchor(
    repo_id: Uuid,
    claim_id: Uuid,
    identity: AnchorIdentity,
    locator: Locator,
    anchored_text: impl Into<String>,
    context: Option<&str>,
    alias_of: Option<String>,
    ingested_at: &str,
    source_time: Option<String>,
) -> EvidenceAnchor {
    let anchored_text = anchored_text.into();
    EvidenceAnchor {
        id: Uuid::new_v4(),
        repo_id,
        claim_id,
        content_hash: content_hash(&anchored_text),
        context_hash: context.map(content_hash),
        identity,
        locator,
        anchored_text,
        alias_of,
        ingested_at: ingested_at.to_string(),
        source_time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_is_whitespace_stable() {
        assert_eq!(normalize_ws("  a\t b\n c "), "a b c");
        assert_eq!(content_hash("a b c"), content_hash("a  b\nc"));
    }

    #[test]
    fn normalize_preserves_case() {
        assert_ne!(content_hash("Foo"), content_hash("foo"));
    }

    #[test]
    fn token_overlap_ranges() {
        assert_eq!(token_overlap("a b c", "a b c"), 1.0);
        assert_eq!(token_overlap("a b c d", "a b"), 0.5);
        assert_eq!(token_overlap("x y", "a b"), 0.0);
    }
}
