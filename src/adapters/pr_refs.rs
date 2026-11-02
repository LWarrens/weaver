//! Pull request identification from git history (REQ-032).
//!
//! Pure parsing, no I/O. Weaver derives PRs from the merge and squash commits a
//! forge leaves behind, so PR ingestion works offline and on any host; the
//! GitHub API only ever *enriches* what is found here.

/// A pull request identified from a commit's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    /// PR/MR number when the message carries one.
    pub number: Option<u64>,
    /// Source branch when the message names one.
    pub branch: Option<String>,
    /// Best-effort title derived from the message.
    pub title: Option<String>,
    /// True when the ref comes only from a `(#N)` subject suffix. That is a
    /// convention, not proof: the number could be an issue. Merge-commit forms
    /// name a pull request outright and are never `squash`.
    pub squash: bool,
}

impl PrRef {
    /// Canonical dedup key: the number when known, else the branch name.
    /// `None` when the ref carries neither (never produced by the parser).
    pub fn key(&self) -> Option<String> {
        self.number
            .map(|n| n.to_string())
            .or_else(|| self.branch.clone())
    }
}

/// Read a run of ASCII digits at the start of `s`.
fn leading_number(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Identify a pull request from a commit message.
///
/// Recognises the merge-commit forms left by GitHub, Bitbucket, and GitLab, and
/// the squash form (`title (#123)`). To avoid mistaking a plain issue reference
/// for a PR, the squash form is only accepted when `(#N)` sits at the very end
/// of the subject; merge forms are accepted on their subject wording, and a
/// two-parent commit is treated as corroborating evidence.
pub fn parse_pr_from_commit(subject: &str, body: &str, parent_count: usize) -> Option<PrRef> {
    let subject = subject.trim();

    // --- GitHub / Bitbucket: "Merge pull request #123 from org/branch" -------
    if let Some(rest) = subject.strip_prefix("Merge pull request #") {
        let number = leading_number(rest);
        let branch = rest.split(" from ").nth(1).map(|b| {
            // Bitbucket appends " to <target>"; keep only the source branch.
            let b = b.split(" to ").next().unwrap_or(b).trim();
            b.to_string()
        });
        return Some(PrRef {
            number,
            branch,
            title: first_non_empty_line(body),
            squash: false,
        });
    }

    // --- Bitbucket alternate: "Merged in branch (pull request #123)" ---------
    if let Some(rest) = subject.strip_prefix("Merged in ") {
        if let Some(idx) = rest.find("(pull request #") {
            let number = leading_number(&rest[idx + "(pull request #".len()..]);
            let branch = rest[..idx].trim();
            return Some(PrRef {
                number,
                branch: (!branch.is_empty()).then(|| branch.to_string()),
                title: first_non_empty_line(body),
                squash: false,
            });
        }
    }

    // --- GitLab: subject "Merge branch 'x' into 'main'", body "See merge request org/repo!12"
    if let Some(idx) = body.find("See merge request") {
        let after = &body[idx..];
        if let Some(bang) = after.find('!') {
            if let Some(number) = leading_number(&after[bang + 1..]) {
                let branch = subject
                    .strip_prefix("Merge branch '")
                    .and_then(|r| r.split('\'').next())
                    .map(str::to_string);
                return Some(PrRef {
                    number: Some(number),
                    branch,
                    title: first_non_empty_line(body),
                    squash: false,
                });
            }
        }
    }

    // --- Squash: "Add response cache (#8)" -----------------------------------
    if subject.ends_with(')') {
        if let Some(idx) = subject.rfind("(#") {
            let inner = &subject[idx + 2..subject.len() - 1];
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(number) = inner.parse::<u64>() {
                    let title = subject[..idx].trim();
                    return Some(PrRef {
                        number: Some(number),
                        branch: None,
                        title: (!title.is_empty()).then(|| title.to_string()),
                        squash: true,
                    });
                }
            }
        }
    }

    // A two-parent commit with no recognisable PR wording is an ordinary merge.
    let _ = parent_count;
    None
}

/// Extract `(owner, repo)` from a GitHub remote URL. Returns `None` for any
/// other host, which is what puts PR sync into git-only mode.
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    let rest = if let Some(r) = url.strip_prefix("git@github.com:") {
        r
    } else if let Some(r) = url.strip_prefix("ssh://git@github.com/") {
        r
    } else if let Some(r) = url.strip_prefix("https://github.com/") {
        r
    } else if let Some(r) = url.strip_prefix("http://github.com/") {
        r
    } else {
        return None;
    };

    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_merge_commit() {
        let pr = parse_pr_from_commit(
            "Merge pull request #123 from acme/feat-cache",
            "Add a response cache\n\nDetails here.",
            2,
        )
        .expect("parsed");
        assert_eq!(pr.number, Some(123));
        assert_eq!(pr.branch.as_deref(), Some("acme/feat-cache"));
        assert_eq!(pr.title.as_deref(), Some("Add a response cache"));
        assert_eq!(pr.key().as_deref(), Some("123"));
    }

    #[test]
    fn parses_bitbucket_merge_forms() {
        let pr = parse_pr_from_commit(
            "Merge pull request #7 in WEAV/weaver from feat-x to master",
            "",
            2,
        )
        .expect("parsed");
        assert_eq!(pr.number, Some(7));
        assert_eq!(pr.branch.as_deref(), Some("feat-x"));

        let pr2 = parse_pr_from_commit("Merged in feat-y (pull request #9)", "", 2).expect("parsed");
        assert_eq!(pr2.number, Some(9));
        assert_eq!(pr2.branch.as_deref(), Some("feat-y"));
    }

    #[test]
    fn parses_gitlab_merge_request() {
        let pr = parse_pr_from_commit(
            "Merge branch 'feat-z' into 'main'",
            "Add z\n\nSee merge request acme/weaver!42",
            2,
        )
        .expect("parsed");
        assert_eq!(pr.number, Some(42));
        assert_eq!(pr.branch.as_deref(), Some("feat-z"));
    }

    #[test]
    fn parses_squash_commit() {
        let pr = parse_pr_from_commit("Add response cache (#8)", "body", 1).expect("parsed");
        assert_eq!(pr.number, Some(8));
        assert_eq!(pr.branch, None);
        assert_eq!(pr.title.as_deref(), Some("Add response cache"));
        assert!(pr.squash, "a `(#N)` suffix is unverified");
    }

    #[test]
    fn ignores_non_trailing_issue_reference() {
        // A mid-subject issue reference is not a squash-merge marker.
        assert!(parse_pr_from_commit("Fix (#12) regression in cache", "", 1).is_none());
        assert!(parse_pr_from_commit("Ordinary commit", "", 1).is_none());
        // A plain two-parent merge with no PR wording is not a PR.
        assert!(parse_pr_from_commit("Merge branch 'main' into feat", "", 2).is_none());
    }

    #[test]
    fn parses_github_remote_forms() {
        let expected = ("acme".to_string(), "weaver".to_string());
        assert_eq!(parse_github_remote("git@github.com:acme/weaver.git"), Some(expected.clone()));
        assert_eq!(parse_github_remote("https://github.com/acme/weaver"), Some(expected.clone()));
        assert_eq!(parse_github_remote("https://github.com/acme/weaver.git"), Some(expected.clone()));
        assert_eq!(parse_github_remote("ssh://git@github.com/acme/weaver.git"), Some(expected));
    }

    #[test]
    fn rejects_non_github_remotes() {
        assert_eq!(parse_github_remote("git@gitlab.com:acme/weaver.git"), None);
        assert_eq!(parse_github_remote("https://bitbucket.org/acme/weaver"), None);
        assert_eq!(parse_github_remote("https://github.com/acme"), None);
        assert_eq!(parse_github_remote(""), None);
    }
}
