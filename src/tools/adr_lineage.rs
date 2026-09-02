use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::entities::AdrDocument;
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn default_max_hops() -> usize {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AdrLineageParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// ADR ID to start from, e.g. "ADR-0003", "ADR-3", "0003", or "3".
    pub adr_id: String,
    /// How many hops to follow in each direction (default: 10).
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct LineageNode {
    pub adr_id: String,
    pub title: Option<String>,
    pub status: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AdrLineageResult {
    pub root: LineageNode,
    /// ADRs that this one superseded (predecessors), ordered oldest→newest.
    pub superseded: Vec<LineageNode>,
    /// ADRs that superseded this one (successors), ordered oldest→newest.
    pub superseded_by: Vec<LineageNode>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: AdrLineageParams,
) -> Result<AdrLineageResult, Error> {
    if params.adr_id.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "adr_id",
            reason: "must not be empty".to_string(),
        });
    }

    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
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

    let repo = store.upsert_repository(repo_path_str, None).await?;

    let mut warnings = Vec::new();

    let root_doc = store
        .find_adr_by_adr_id_flex(repo.id, &params.adr_id)
        .await?;

    let (root_node, superseded, superseded_by) = if let Some(doc) = root_doc {
        let preds = store
            .get_supersession_predecessors(doc.id, params.max_hops)
            .await?;
        let succs = store
            .get_supersession_successors(doc.id, params.max_hops)
            .await?;

        let root = adr_to_node(doc);
        let superseded = preds.into_iter().map(adr_to_node).collect();
        let superseded_by = succs.into_iter().map(adr_to_node).collect();
        (root, superseded, superseded_by)
    } else {
        warnings.push(format!(
            "ADR '{}' not found in repository",
            params.adr_id
        ));
        let root = LineageNode {
            adr_id: params.adr_id,
            title: None,
            status: "unknown".to_string(),
            valid_from: String::new(),
            valid_to: None,
        };
        (root, vec![], vec![])
    };

    Ok(AdrLineageResult {
        root: root_node,
        superseded,
        superseded_by,
        warnings,
    })
}

fn adr_to_node(doc: AdrDocument) -> LineageNode {
    LineageNode {
        adr_id: doc.adr_id,
        title: Some(doc.title),
        status: doc.status.as_str().to_string(),
        valid_from: doc.valid_from,
        valid_to: doc.valid_to,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::{AdrDocument, AdrStatus};
    use crate::storage::SqliteStore;
    use uuid::Uuid;

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    fn make_adr(repo_id: Uuid, adr_id: &str, title: &str, status: AdrStatus) -> AdrDocument {
        let now = "2024-01-01T00:00:00Z".to_string();
        AdrDocument {
            id: Uuid::new_v4(),
            repo_id,
            adr_id: adr_id.to_string(),
            title: title.to_string(),
            status,
            date: None,
            context: None,
            decision: None,
            consequences: None,
            supersedes: vec![],
            superseded_by: None,
            file_mentions: vec![],
            service_mentions: vec![],
            module_mentions: vec![],
            source_uri: format!("docs/adr/{}.md", adr_id.to_lowercase()),
            effective_from: None,
            effective_to: None,
            valid_from: now.clone(),
            valid_to: None,
            ingested_at: now,
            source_time: None,
            confidence: 1.0,
        }
    }

    #[tokio::test]
    async fn adr_lineage_returns_chain() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().canonicalize().expect("canonicalize");
        let repo_path_str = repo_path.to_str().unwrap();

        let repo = store
            .upsert_repository(repo_path_str, None)
            .await
            .expect("repo");

        let adr1 = make_adr(repo.id, "ADR-0001", "Use SQLite", AdrStatus::Superseded);
        let adr2 = make_adr(repo.id, "ADR-0002", "Use PostgreSQL", AdrStatus::Superseded);
        let adr3 = make_adr(repo.id, "ADR-0003", "Use DuckDB", AdrStatus::Accepted);

        store.insert_adr(&adr1).await.expect("insert adr1");
        store.insert_adr(&adr2).await.expect("insert adr2");
        store.insert_adr(&adr3).await.expect("insert adr3");

        let now = "2024-01-01T00:00:00Z";
        // ADR-0002 supersedes ADR-0001
        store
            .upsert_supersession_edge(adr2.id, adr1.id, now, now)
            .await
            .expect("edge 2->1");
        // ADR-0003 supersedes ADR-0002
        store
            .upsert_supersession_edge(adr3.id, adr2.id, now, now)
            .await
            .expect("edge 3->2");

        let result = run(
            &store,
            AdrLineageParams {
                repo_path: repo_path_str.to_string(),
                adr_id: "ADR-0002".to_string(),
                max_hops: 10,
            },
        )
        .await
        .expect("lineage");

        assert_eq!(result.root.adr_id, "ADR-0002");
        assert!(result.warnings.is_empty());
        assert_eq!(result.superseded.len(), 1);
        assert_eq!(result.superseded[0].adr_id, "ADR-0001");
        assert_eq!(result.superseded_by.len(), 1);
        assert_eq!(result.superseded_by[0].adr_id, "ADR-0003");
    }

    #[tokio::test]
    async fn adr_lineage_unknown_adr_returns_warning() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().canonicalize().expect("canonicalize");

        let result = run(
            &store,
            AdrLineageParams {
                repo_path: repo_path.to_str().unwrap().to_string(),
                adr_id: "ADR-9999".to_string(),
                max_hops: 10,
            },
        )
        .await
        .expect("lineage");

        assert_eq!(result.root.adr_id, "ADR-9999");
        assert!(!result.warnings.is_empty());
        assert!(result.warnings[0].contains("not found"));
        assert!(result.superseded.is_empty());
        assert!(result.superseded_by.is_empty());
    }
}
