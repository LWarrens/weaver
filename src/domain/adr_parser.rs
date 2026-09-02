//! Deterministic ADR markdown parser.
//!
//! Parses both the "Michael Nygard" format and the MADR format by walking
//! pulldown-cmark events and tracking which H2 section is active.
//!
//! Contract:
//! - Missing fields are preserved as `None` — never invented.
//! - ADR ID is extracted from the title heading or from the filename.
//! - Status is extracted from a paragraph in the Status section.
//! - Supersession links are extracted from the Status section text or Links section.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::domain::entities::AdrStatus;

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ParsedAdr {
    /// Canonical ADR identifier extracted from title or filename, e.g. "ADR-0042".
    pub adr_id: Option<String>,
    pub title: String,
    pub status: Option<AdrStatus>,
    /// Date string from the ADR header (ISO-8601 date or free-form).
    pub date: Option<String>,
    pub context: Option<String>,
    pub decision: Option<String>,
    pub consequences: Option<String>,
    /// ADR IDs this document explicitly supersedes.
    pub supersedes: Vec<String>,
    /// ADR ID this document is superseded by.
    pub superseded_by: Option<String>,
    /// File/module/service paths mentioned explicitly in the ADR text.
    pub file_mentions: Vec<String>,
    pub service_mentions: Vec<String>,
    pub module_mentions: Vec<String>,
    /// Constraint statements extracted from the decision and consequences text.
    pub constraints: Vec<ExtractedConstraint>,
}

#[derive(Debug, Clone)]
pub struct ExtractedConstraint {
    pub text: String,
    /// 0.65 for heuristically extracted constraints (contains "must", "shall", etc.)
    pub confidence: f64,
    /// ADR section the constraint sentence was extracted from ("Decision" or
    /// "Consequences"). Used to anchor the constraint claim to that section.
    pub section: String,
}

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Section {
    Header,
    Status,
    Context,
    Decision,
    Consequences,
    Links,
    Other,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parse an ADR markdown document.
///
/// `filename_stem` is used as a fallback ADR ID source (e.g. "0042-use-event-sourcing").
pub fn parse(source: &str, filename_stem: &str) -> ParsedAdr {
    let mut adr = ParsedAdr::default();
    let mut section = Section::Header;
    let mut text_buf = String::new();
    let mut heading_level = HeadingLevel::H1;
    // Accumulate per-section text
    let mut context_buf = String::new();
    let mut decision_buf = String::new();
    let mut consequences_buf = String::new();
    let mut status_buf = String::new();
    let mut links_buf = String::new();
    let opts = Options::empty();
    let parser = Parser::new_ext(source, opts);

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                heading_level = level;
                text_buf.clear();
            }

            Event::End(TagEnd::Heading(_)) => {
                let heading_text = text_buf.trim().to_string();
                text_buf.clear();

                if heading_level == HeadingLevel::H1 {
                    // Title
                    let (id, title) = split_adr_title(&heading_text);
                    adr.adr_id = id;
                    adr.title = title;
                } else if heading_level == HeadingLevel::H2 {
                    // Section transition
                    section = classify_section(&heading_text);
                }
            }

            Event::Start(Tag::Paragraph) => {
                text_buf.clear();
            }

            Event::End(TagEnd::Paragraph) => {
                let para = text_buf.trim().to_string();
                text_buf.clear();

                match section {
                    Section::Status => {
                        status_buf.push_str(&para);
                        status_buf.push('\n');
                    }
                    Section::Context => {
                        context_buf.push_str(&para);
                        context_buf.push('\n');
                    }
                    Section::Decision => {
                        decision_buf.push_str(&para);
                        decision_buf.push('\n');
                    }
                    Section::Consequences => {
                        consequences_buf.push_str(&para);
                        consequences_buf.push('\n');
                    }
                    Section::Links => {
                        links_buf.push_str(&para);
                        links_buf.push('\n');
                    }
                    _ => {}
                }
            }

            Event::Start(Tag::Item) => {
                text_buf.clear();
            }

            Event::End(TagEnd::Item) => {
                let item = text_buf.trim().to_string();
                text_buf.clear();

                match section {
                    Section::Status => {
                        status_buf.push_str(&item);
                        status_buf.push('\n');
                    }
                    Section::Context => {
                        context_buf.push_str(&item);
                        context_buf.push('\n');
                    }
                    Section::Decision => {
                        decision_buf.push_str(&item);
                        decision_buf.push('\n');
                    }
                    Section::Consequences => {
                        consequences_buf.push_str(&item);
                        consequences_buf.push('\n');
                    }
                    Section::Links => {
                        links_buf.push_str(&item);
                        links_buf.push('\n');
                    }
                    _ => {}
                }
            }

            Event::Text(t) => {
                text_buf.push_str(&t);
            }

            // Inline code spans: re-wrap with backticks so extract_file_mentions
            // can detect path-like tokens (e.g. `src/storage/db.rs`).
            Event::Code(t) => {
                text_buf.push('`');
                text_buf.push_str(&t);
                text_buf.push('`');
            }

            Event::SoftBreak | Event::HardBreak => {
                text_buf.push('\n');
            }

            _ => {}
        }
    }

    // Finalise sections
    if !status_buf.is_empty() {
        adr.status = extract_status(&status_buf);
        if let Some(date) = extract_date_from_status(&status_buf) {
            adr.date = Some(date);
        }
        extract_supersession_from_text(&status_buf, &mut adr);
    }

    if !context_buf.is_empty() {
        adr.context = Some(context_buf.trim().to_string());
        extract_file_mentions(&context_buf, &mut adr.file_mentions);
    }
    if !decision_buf.is_empty() {
        adr.decision = Some(decision_buf.trim().to_string());
        extract_constraints_from_text(&decision_buf, 0.65, "Decision", &mut adr.constraints);
        extract_file_mentions(&decision_buf, &mut adr.file_mentions);
    }
    if !consequences_buf.is_empty() {
        adr.consequences = Some(consequences_buf.trim().to_string());
        extract_constraints_from_text(&consequences_buf, 0.65, "Consequences", &mut adr.constraints);
        extract_file_mentions(&consequences_buf, &mut adr.file_mentions);
    }
    if !links_buf.is_empty() {
        extract_supersession_from_text(&links_buf, &mut adr);
    }

    // Fallback ADR ID from filename if not found in title
    if adr.adr_id.is_none() {
        adr.adr_id = adr_id_from_filename(filename_stem);
    }

    adr
}

// ---------------------------------------------------------------------------
// Section classification
// ---------------------------------------------------------------------------

fn classify_section(heading: &str) -> Section {
    let lower = heading.to_lowercase();
    if lower.starts_with("status") {
        Section::Status
    } else if lower.starts_with("context") {
        Section::Context
    } else if lower.starts_with("decision") {
        Section::Decision
    } else if lower.starts_with("consequences")
        || lower.starts_with("positive consequences")
        || lower.starts_with("negative consequences")
    {
        Section::Consequences
    } else if lower.starts_with("links") || lower.starts_with("related") {
        Section::Links
    } else {
        Section::Other
    }
}

// ---------------------------------------------------------------------------
// Title parsing
// ---------------------------------------------------------------------------

/// Split a title heading into (adr_id, title).
///
/// Handled formats:
/// - "ADR-0042: Use Event Sourcing"  → (Some("ADR-0042"), "Use Event Sourcing")
/// - "ADR 42: Title"                 → (Some("ADR-0042"), "Title")
/// - "0042: Title"                   → (Some("ADR-0042"), "Title")
/// - "42. Title"                     → (Some("ADR-0042"), "Title")
/// - "Title"                         → (None, "Title")
fn split_adr_title(text: &str) -> (Option<String>, String) {
    // Pattern: "ADR-NNNN: ..." or "ADR NNNN: ..."
    if let Some(rest) = text.strip_prefix("ADR") {
        let rest = rest.trim_start_matches(|c| c == '-' || c == ' ');
        // rest should start with digits
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let after_digits = &rest[digits.len()..];
            let title = after_digits
                .trim_start_matches(|c| c == ':' || c == '-' || c == ' ')
                .to_string();
            let n: u32 = digits.parse().unwrap_or(0);
            let id = format!("ADR-{:04}", n);
            return (
                Some(id),
                if title.is_empty() {
                    text.to_string()
                } else {
                    title
                },
            );
        }
    }

    // Pattern: "NNNN: ..." or "NNNN. ..."
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 3 {
        let after = &text[digits.len()..];
        let title = after
            .trim_start_matches(|c| c == ':' || c == '.' || c == '-' || c == ' ')
            .to_string();
        let n: u32 = digits.parse().unwrap_or(0);
        let id = format!("ADR-{:04}", n);
        return (
            Some(id),
            if title.is_empty() {
                text.to_string()
            } else {
                title
            },
        );
    }

    (None, text.to_string())
}

/// Attempt to derive an ADR ID from a filename stem.
///
/// "0042-use-event-sourcing" → Some("ADR-0042")
/// "adr-0042-title"          → Some("ADR-0042")
fn adr_id_from_filename(stem: &str) -> Option<String> {
    // Strip leading "adr-" prefix (case-insensitive)
    let stem = stem
        .strip_prefix("adr-")
        .or_else(|| stem.strip_prefix("ADR-"))
        .unwrap_or(stem);

    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 3 {
        let n: u32 = digits.parse().ok()?;
        Some(format!("ADR-{:04}", n))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Status extraction
// ---------------------------------------------------------------------------

fn extract_status(text: &str) -> Option<AdrStatus> {
    for line in text.lines() {
        let lower = line.trim().to_lowercase();
        // Strip list markers and metadata keys like "Status: accepted"
        let value = lower
            .trim_start_matches('-')
            .trim_start_matches('*')
            .trim()
            .trim_start_matches("status:")
            .trim()
            .trim_start_matches("status")
            .trim();

        match value {
            "accepted" | "approved" => return Some(AdrStatus::Accepted),
            "proposed" | "draft" => return Some(AdrStatus::Proposed),
            "deprecated" => return Some(AdrStatus::Deprecated),
            "superseded" => return Some(AdrStatus::Superseded),
            "rejected" => return Some(AdrStatus::Rejected),
            _ => {}
        }

        // Also match "Accepted" at start of line (MADR style: just the word)
        let first_word = value.split_whitespace().next().unwrap_or("");
        match first_word {
            "accepted" | "approved" => return Some(AdrStatus::Accepted),
            "proposed" | "draft" => return Some(AdrStatus::Proposed),
            "deprecated" => return Some(AdrStatus::Deprecated),
            "superseded" => return Some(AdrStatus::Superseded),
            "rejected" => return Some(AdrStatus::Rejected),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Date extraction
// ---------------------------------------------------------------------------

fn extract_date_from_status(text: &str) -> Option<String> {
    for line in text.lines() {
        let lower = line.trim().to_lowercase();
        // "Date: 2023-04-10" or "- Date: 2023-04-10"
        let normalized = lower.trim_start_matches('-').trim_start_matches('*').trim();
        if let Some(rest) = normalized.strip_prefix("date:").or_else(|| {
            normalized
                .find("date:")
                .map(|index| &normalized[index + 5..])
        }) {
            let date = rest
                .trim()
                .trim_end_matches(|c: char| c == '.' || c == ',' || c == ';')
                .to_string();
            if looks_like_date(&date) {
                return Some(date);
            }
        }
    }
    None
}

fn looks_like_date(s: &str) -> bool {
    // Very loose check: YYYY-MM-DD or YYYY/MM/DD
    let s = s.trim();
    s.len() >= 8
        && s.chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Supersession extraction
// ---------------------------------------------------------------------------

fn extract_supersession_from_text(text: &str, adr: &mut ParsedAdr) {
    let lower = text.to_lowercase();

    // Scan for all "superseded by" occurrences and capture the ADR ID that follows.
    let mut start = 0;
    while let Some(pos) = lower[start..].find("superseded by") {
        let abs = start + pos;
        let after = &lower[abs + "superseded by".len()..];
        if let Some(id) = extract_adr_ref(after) {
            adr.superseded_by = Some(id);
        }
        start = abs + 1;
    }

    // Scan for "supersedes" occurrences. "superseded" ends in 'd', "supersedes" in 's',
    // so find("supersedes") will NOT match occurrences of "superseded by".
    let mut start = 0;
    while let Some(pos) = lower[start..].find("supersedes") {
        let abs = start + pos;
        let after = &lower[abs + "supersedes".len()..];
        if let Some(id) = extract_adr_ref(after) {
            if !adr.supersedes.contains(&id) {
                adr.supersedes.push(id);
            }
        }
        start = abs + 1;
    }
}

/// Extract an ADR ID like "ADR-0042" from arbitrary text.
fn extract_adr_ref(text: &str) -> Option<String> {
    // Look for pattern: adr-NNNN or adr NNNN (case-insensitive, already lowercased)
    let text = text.to_lowercase();
    if let Some(pos) = text.find("adr") {
        let after = &text[pos + 3..];
        let after = after.trim_start_matches(|c: char| c == '-' || c == ' ' || c == '[');
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let n: u32 = digits.parse().ok()?;
            return Some(format!("ADR-{:04}", n));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// File mention extraction
// ---------------------------------------------------------------------------

/// Recognized source file extensions for file-path heuristics.
const SOURCE_EXTENSIONS: &[&str] = &[
    ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".kt", ".cs", ".cpp", ".c", ".h",
    ".rb", ".swift", ".scala", ".sh", ".yaml", ".yml", ".toml", ".json", ".md", ".sql",
];

/// Extract backtick-quoted file paths from `text` and append to `out`.
///
/// Only backtick-quoted tokens are considered (e.g. `` `src/db.rs` ``).
/// The string must contain a `/` and end with a recognized source extension.
fn extract_file_mentions(text: &str, out: &mut Vec<String>) {
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '`' {
            let mut span = String::new();
            for (_, cc) in chars.by_ref() {
                if cc == '`' {
                    break;
                }
                span.push(cc);
            }
            if looks_like_file_path(&span) {
                let normalized = span.replace('\\', "/");
                if !out.contains(&normalized) {
                    out.push(normalized);
                }
            }
        }
    }
}

fn looks_like_file_path(s: &str) -> bool {
    if !s.contains('/') {
        return false;
    }
    // Exclude URLs
    if s.starts_with("http://") || s.starts_with("https://") {
        return false;
    }
    let lower = s.to_lowercase();
    SOURCE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

// ---------------------------------------------------------------------------
// Constraint extraction
// ---------------------------------------------------------------------------

/// Extract constraint statements from a block of text.
///
/// Heuristic: sentences or bullet points containing obligation keywords.
fn extract_constraints_from_text(
    text: &str,
    confidence: f64,
    section: &str,
    out: &mut Vec<ExtractedConstraint>,
) {
    const OBLIGATION_KEYWORDS: &[&str] = &[
        "must not",
        "must ",
        "shall not",
        "shall ",
        "required to",
        "is prohibited",
        "are prohibited",
        "never ",
        "always ",
        "cannot ",
        "may not",
    ];

    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches(['-', '*', '•']).trim();
        if trimmed.is_empty() || trimmed.len() < 10 {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if OBLIGATION_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            out.push(ExtractedConstraint {
                text: trimmed.to_string(),
                confidence,
                section: section.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nygard_format() {
        let src = r#"# ADR-0001: Use SQLite

## Status

Accepted

## Context

We need persistent storage.

## Decision

We will use SQLite with WAL mode.

## Consequences

- Files must not be opened by multiple writers simultaneously.
- All timestamps must be stored as TEXT in ISO-8601 format.
"#;
        let adr = parse(src, "0001-use-sqlite");
        assert_eq!(adr.adr_id, Some("ADR-0001".to_string()));
        assert_eq!(adr.title, "Use SQLite");
        assert_eq!(adr.status, Some(AdrStatus::Accepted));
        assert!(adr.decision.is_some());
        assert!(!adr.constraints.is_empty());
    }

    #[test]
    fn extracts_supersession_links() {
        let src = r#"# ADR-0042: New Decision

## Status

Accepted. Supersedes ADR-0010. Superseded by ADR-0050.
"#;
        let adr = parse(src, "0042-new-decision");
        assert_eq!(adr.supersedes, vec!["ADR-0010"]);
        assert_eq!(adr.superseded_by, Some("ADR-0050".to_string()));
    }

    #[test]
    fn id_from_filename_fallback() {
        let src = "# Title\n\n## Status\n\nAccepted\n";
        let adr = parse(src, "0099-some-decision");
        assert_eq!(adr.adr_id, Some("ADR-0099".to_string()));
    }

    #[test]
    fn handles_missing_sections_gracefully() {
        let src = "# ADR-0001: Minimal\n";
        let adr = parse(src, "0001-minimal");
        assert_eq!(adr.adr_id, Some("ADR-0001".to_string()));
        assert_eq!(adr.title, "Minimal");
        assert!(adr.status.is_none());
        assert!(adr.context.is_none());
        assert!(adr.decision.is_none());
    }
}
