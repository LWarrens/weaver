use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::{GraphEdgeRow, GraphNodeRow, SqliteStore};
use crate::tools::json_utils::de as num_de;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetGraphSnapshotParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
    /// When set, return only the neighbourhood of matching symbol(s) instead of
    /// a sampled global snapshot. Accepts exact name or substring. No edge cap.
    #[serde(default)]
    pub focus_symbol: Option<String>,
    /// Hops to expand from focus_symbol. Defaults to 2. Only used when focus_symbol is set.
    #[serde(default = "default_focus_depth", deserialize_with = "num_de::u32_or_str")]
    pub focus_depth: u32,
    /// Max nodes fetched per major kind (global snapshot only). Omit for unlimited.
    #[serde(default, deserialize_with = "num_de::opt_usize")]
    pub max_nodes_per_kind: Option<usize>,
    /// Max edge rows fetched for each global edge batch. Omit for unlimited.
    #[serde(default, deserialize_with = "num_de::opt_usize")]
    pub max_edges: Option<usize>,
}

fn default_focus_depth() -> u32 {
    2
}

#[derive(Debug, Serialize)]
pub struct GetGraphSnapshotResult {
    pub repo_path: String,
    pub nodes: Vec<GraphNodeRow>,
    pub edges: Vec<GraphEdgeRow>,
    pub warnings: Vec<String>,
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: GetGraphSnapshotParams,
) -> Result<GetGraphSnapshotResult, Error> {
    let now = Utc::now().to_rfc3339();
    let valid_at = params.valid_at.as_deref().unwrap_or(&now);

    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!(
            "path does not exist or is not accessible: {}",
            params.repo_path
        ),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    let repo = store.upsert_repository(repo_path_str, None).await?;

    let (nodes, edges) = if let Some(ref focus) = params.focus_symbol {
        store
            .fetch_focused_snapshot(
                repo.id,
                focus,
                params.focus_depth.clamp(1, 6),
                valid_at,
            )
            .await?
    } else {
        store
            .fetch_graph_snapshot(
                repo.id,
                valid_at,
                params.max_nodes_per_kind,
                params.max_edges,
            )
            .await?
    };

    let mut warnings = Vec::new();
    if nodes.is_empty() {
        if params.focus_symbol.is_some() {
            warnings.push(format!(
                "no symbols matching '{}' found; run ingest_symbols first",
                params.focus_symbol.as_deref().unwrap_or("")
            ));
        } else {
            warnings.push("graph is empty; ingest ADRs, symbols, or commits first".to_string());
        }
    }

    Ok(GetGraphSnapshotResult {
        repo_path: repo_path_str.to_string(),
        nodes,
        edges,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_empty_graph_with_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");
        let store = Arc::new(store);

        let result = run(
            &store,
            GetGraphSnapshotParams {
                repo_path: dir.path().to_string_lossy().to_string(),
                valid_at: None,
                focus_symbol: None,
                focus_depth: default_focus_depth(),
                max_nodes_per_kind: Some(10),
                max_edges: Some(50),
            },
        )
        .await
        .expect("graph snapshot");

        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn omitted_max_fields_are_unlimited() {
        let params: GetGraphSnapshotParams =
            serde_json::from_value(serde_json::json!({ "repo_path": "." }))
                .expect("params deserialize");

        assert_eq!(params.max_nodes_per_kind, None);
        assert_eq!(params.max_edges, None);
    }
}
