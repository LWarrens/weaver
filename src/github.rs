//! Optional GitHub API enrichment for pull request ingestion (REQ-032).
//!
//! PR sync derives its baseline from git history alone. This client only fills
//! in what a merge commit cannot carry — the PR body, author, state, and review
//! discussion. Every failure here is a warning, never a hard error: a run
//! without a token, without network, or against a non-GitHub remote still
//! ingests PRs, just without enrichment.

use serde::Deserialize;

/// PR detail fetched from the GitHub API.
#[derive(Debug, Clone, Default)]
pub struct PrDetail {
    pub number: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub author: Option<String>,
    pub state: Option<String>,
    pub url: Option<String>,
    /// Review bodies, oldest first. Feeds decision mining alongside the PR body.
    pub review_bodies: Vec<String>,
}

// --- wire types -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PullPayload {
    number: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    user: Option<UserPayload>,
}

#[derive(Debug, Deserialize)]
struct UserPayload {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewPayload {
    #[serde(default)]
    body: Option<String>,
}

impl PullPayload {
    fn into_detail(self) -> PrDetail {
        // A merged PR reports state "closed"; surface the more useful distinction.
        let state = if self.merged_at.is_some() {
            Some("merged".to_string())
        } else {
            self.state
        };
        PrDetail {
            number: self.number,
            title: self.title,
            body: self.body,
            author: self.user.and_then(|u| u.login),
            state,
            url: self.html_url,
            review_bodies: Vec::new(),
        }
    }
}

// --- client -----------------------------------------------------------------

pub struct GitHubClient {
    token: String,
    base_url: String,
    client: reqwest::Client,
}

impl GitHubClient {
    pub fn new(token: String, base_url: String) -> Self {
        Self {
            token,
            base_url,
            client: reqwest::Client::new(),
        }
    }

    /// Build a client from the environment. `None` when no token is configured,
    /// which puts PR sync into git-only mode.
    ///
    /// - `WEAVER_GITHUB_TOKEN` (falls back to `GITHUB_TOKEN`)
    /// - `WEAVER_GITHUB_API_BASE` (default `https://api.github.com`; also covers
    ///   GitHub Enterprise and is the seam tests point at a local stub)
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("WEAVER_GITHUB_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .ok()
            .filter(|t| !t.trim().is_empty())?;
        let base_url = std::env::var("WEAVER_GITHUB_API_BASE")
            .unwrap_or_else(|_| "https://api.github.com".to_string());
        Some(Self::new(token, base_url))
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "weaver")
            .bearer_auth(&self.token)
    }

    /// Look up `number` as a pull request, telling a PR from an issue.
    ///
    /// GitHub answers 404 for a missing number *and* for a repository the token
    /// cannot see, so a 404 alone proves nothing. On 404 this probes
    /// `/issues/{number}`: a visible issue without a `pull_request` field is
    /// definitively not a PR; anything else stays [`PrLookup::Unresolved`].
    pub async fn lookup_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> anyhow::Result<PrLookup> {
        let base = self.base_url.trim_end_matches('/');
        let resp = self
            .get(&format!("{base}/repos/{owner}/{repo}/pulls/{number}"))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let probe = self
                .get(&format!("{base}/repos/{owner}/{repo}/issues/{number}"))
                .send()
                .await?;
            if !probe.status().is_success() {
                return Ok(PrLookup::Unresolved);
            }
            let issue: serde_json::Value = probe.json().await?;
            return Ok(if issue.get("pull_request").is_none() {
                PrLookup::NotAPr
            } else {
                PrLookup::Unresolved
            });
        }

        let pull: PullPayload = resp.error_for_status()?.json().await?;
        let mut detail = pull.into_detail();

        // Reviews are supplementary: a failure here must not lose the PR body.
        if let Ok(resp) = self
            .get(&format!("{base}/repos/{owner}/{repo}/pulls/{number}/reviews"))
            .send()
            .await
        {
            if let Ok(reviews) = resp.json::<Vec<ReviewPayload>>().await {
                detail.review_bodies = reviews
                    .into_iter()
                    .filter_map(|r| r.body)
                    .map(|b| b.trim().to_string())
                    .filter(|b| !b.is_empty())
                    .collect();
            }
        }

        Ok(PrLookup::Pr(detail))
    }
}

/// What GitHub says a number is.
#[derive(Debug, Clone)]
pub enum PrLookup {
    /// A pull request, with its detail.
    Pr(PrDetail),
    /// A visible issue that is not a pull request.
    NotAPr,
    /// GitHub could not confirm either way (missing, or not visible to the token).
    Unresolved,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_pull_payload() {
        let json = r#"{
            "number": 42,
            "title": "Add response cache",
            "body": "Implements ADR-0001.",
            "state": "closed",
            "merged_at": "2026-01-02T03:04:05Z",
            "html_url": "https://github.com/acme/weaver/pull/42",
            "user": { "login": "octocat" }
        }"#;
        let detail = serde_json::from_str::<PullPayload>(json).expect("parse").into_detail();
        assert_eq!(detail.number, 42);
        assert_eq!(detail.title.as_deref(), Some("Add response cache"));
        assert_eq!(detail.author.as_deref(), Some("octocat"));
        // merged_at wins over the raw "closed" state.
        assert_eq!(detail.state.as_deref(), Some("merged"));
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let detail = serde_json::from_str::<PullPayload>(r#"{"number": 1}"#)
            .expect("parse")
            .into_detail();
        assert_eq!(detail.number, 1);
        assert!(detail.title.is_none());
        assert!(detail.state.is_none());
        assert!(detail.review_bodies.is_empty());
    }

    #[test]
    fn from_env_requires_a_token() {
        let _guard = crate::env_guard();
        std::env::remove_var("WEAVER_GITHUB_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");
        assert!(GitHubClient::from_env().is_none());

        std::env::set_var("WEAVER_GITHUB_TOKEN", "t0ken");
        let client = GitHubClient::from_env().expect("client");
        assert_eq!(client.base_url, "https://api.github.com");
        std::env::remove_var("WEAVER_GITHUB_TOKEN");
    }
}
