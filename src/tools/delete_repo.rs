use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteRepoParams {
    /// Absolute path to the repository root (must match a stored repository path exactly).
    pub repo_path: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteRepoResult {
    pub deleted: bool,
    pub repo_path: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: DeleteRepoParams,
) -> Result<DeleteRepoResult, Error> {
    let repo_path = params.repo_path.trim().to_string();

    let deleted = store.delete_repository(&repo_path).await?;

    if deleted {
        Ok(DeleteRepoResult {
            deleted: true,
            repo_path,
            message: "Repository and all associated data deleted.".to_string(),
        })
    } else {
        Ok(DeleteRepoResult {
            deleted: false,
            repo_path,
            message: "Repository not found; nothing was deleted.".to_string(),
        })
    }
}
