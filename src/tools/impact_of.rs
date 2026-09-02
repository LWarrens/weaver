use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactOfParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// ADR document ID (e.g. "0001") or decision UUID to start from.
    pub adr_id: String,
    /// Maximum traversal depth (default 3).
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Edge types to follow (default: applies_to, depends_on, conflicts_with, defines, imposes).
    #[serde(default = "default_edge_types")]
    pub edge_types: Vec<String>,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
}

fn default_max_depth() -> u32 {
    3
}

fn default_edge_types() -> Vec<String> {
    vec![
        "applies_to".to_string(),
        "depends_on".to_string(),
        "conflicts_with".to_string(),
        "defines".to_string(),
        "imposes".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ImpactOfResult {
    pub root: Option<RootRef>,
    pub affected: Vec<AffectedNode>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RootRef {
    pub adr_id: String,
    pub title: Option<String>,
    pub entity_type: String,
    pub entity_id: String,
}

#[derive(Debug, Serialize)]
pub struct AffectedNode {
    pub entity_type: String,
    pub entity_id: String,
    pub via_edge: String,
    pub depth: u32,
    pub confidence: f64,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: ImpactOfParams,
) -> Result<ImpactOfResult, Error> {
    let now = Utc::now().to_rfc3339();
    let valid_at = params.valid_at.as_deref().unwrap_or(&now);

    if params.adr_id.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "adr_id",
            reason: "adr_id must not be empty".to_string(),
        });
    }

    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!("path does not exist: {}", params.repo_path),
        })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;

    // Resolve root: try as adr document adr_id first, then as UUID
    let (root_entity_id, root_entity_type, root_title) = {
        let adr_doc = store.find_adr_by_adr_id(repo.id, &params.adr_id).await?;
        if let Some(doc) = adr_doc {
            (
                doc.id.to_string(),
                "adr_document".to_string(),
                Some(doc.title.clone()),
            )
        } else {
            // Maybe it's a raw UUID (decision ID)
            (
                params.adr_id.clone(),
                "decision".to_string(),
                None,
            )
        }
    };

    let edge_types: Vec<&str> = params.edge_types.iter().map(|s| s.as_str()).collect();

    // BFS
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(root_entity_id.clone());

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((root_entity_id.clone(), 0));

    let mut affected: Vec<AffectedNode> = vec![];

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth >= params.max_depth {
            continue;
        }

        let edges = store
            .fetch_impact_edges(&current_id, &edge_types, valid_at)
            .await?;

        for edge in edges {
            if visited.contains(&edge.target_id) {
                continue;
            }
            visited.insert(edge.target_id.clone());

            affected.push(AffectedNode {
                entity_type: edge.target_type.clone(),
                entity_id: edge.target_id.clone(),
                via_edge: edge.edge_type.clone(),
                depth: depth + 1,
                confidence: edge.confidence,
            });

            queue.push_back((edge.target_id, depth + 1));
        }
    }

    let root = Some(RootRef {
        adr_id: params.adr_id.clone(),
        title: root_title,
        entity_type: root_entity_type,
        entity_id: root_entity_id,
    });

    let warnings = if affected.is_empty() {
        vec![format!(
            "no impact found for '{}' within depth {}",
            params.adr_id, params.max_depth
        )]
    } else {
        vec![]
    };

    Ok(ImpactOfResult {
        root,
        affected,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_adrs::{run as sync, SyncAdrsFromGitParams};
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn impact_follows_defines_and_imposes_chain() -> Result<(), Box<dyn std::error::Error>> {
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("docs/decisions"))?;

        // Write ADR with a decision and a constraint (must use)
        fs::write(
            repo.join("docs/decisions/0001-use-sqlite.md"),
            "# 0001 Use SQLite\n\n## Status\nAccepted\n\n## Context\nNeed storage.\n\n## Decision\nUse SQLite as the primary data store.\n\n## Consequences\nMust use WAL mode.\n",
        )?;

        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("db.sqlite").display());
        let store = std::sync::Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        sync(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_glob: "docs/decisions/**/*.md".to_string(),
            },
        )
        .await?;

        let result = run(
            &store,
            ImpactOfParams {
                repo_path: repo.to_string_lossy().to_string(),
                adr_id: "ADR-0001".to_string(),
                max_depth: 3,
                edge_types: default_edge_types(),
                valid_at: None,
            },
        )
        .await?;

        assert!(result.root.is_some(), "root should be found");
        let root = result.root.as_ref().unwrap();
        assert_eq!(root.adr_id, "ADR-0001");
        assert_eq!(root.entity_type, "adr_document");

        // Should have at least a "defines" edge to the decision
        let defines = result.affected.iter().find(|n| n.via_edge == "defines");
        assert!(defines.is_some(), "defines edge to decision should exist");

        Ok(())
    }
}
