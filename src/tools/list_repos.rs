use std::sync::Arc;

use serde::Serialize;

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ListReposResult {
    pub repos: Vec<RepoSummary>,
}

#[derive(Debug, Serialize)]
pub struct RepoSummary {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
    pub ingested_at: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(store: &Arc<SqliteStore>) -> Result<ListReposResult, Error> {
    let repos = store.fetch_all_repositories().await?;

    Ok(ListReposResult {
        repos: repos
            .into_iter()
            .map(|r| RepoSummary {
                id: r.id.to_string(),
                path: r.path,
                name: r.name,
                ingested_at: r.ingested_at,
            })
            .collect(),
    })
}
