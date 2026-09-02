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
pub struct TraceCallPathParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Symbol name to start from. Exact match first, then suffix match.
    pub symbol_name: String,
    /// Traversal direction.
    #[serde(default = "default_direction")]
    pub direction: TraceDirection,
    /// Maximum hops to follow (default 4).
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Prune edges below this confidence (default 0.5).
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TraceDirection {
    Inbound,
    Outbound,
    Both,
}

fn default_direction() -> TraceDirection {
    TraceDirection::Outbound
}
fn default_max_depth() -> u32 {
    4
}
fn default_min_confidence() -> f64 {
    0.5
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TraceCallPathResult {
    pub root: Option<SymbolRef>,
    pub chain: Vec<ChainNode>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolRef {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: Option<i64>,
    /// Compact location anchor: "file:line name"
    pub anchor: String,
}

#[derive(Debug, Serialize)]
pub struct ChainNode {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: Option<i64>,
    pub via_edge: String,
    pub depth: u32,
    pub confidence: f64,
    /// Compact location anchor: "file:line name"
    pub anchor: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: TraceCallPathParams,
) -> Result<TraceCallPathResult, Error> {
    let now = Utc::now().to_rfc3339();
    let valid_at = params.valid_at.as_deref().unwrap_or(&now);

    if params.symbol_name.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "symbol_name",
            reason: "symbol_name must not be empty".to_string(),
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

    // Resolve symbol
    let root = store
        .find_symbol_ref_by_name(repo.id, &params.symbol_name, valid_at)
        .await?;

    let Some(root_ref) = root else {
        return Ok(TraceCallPathResult {
            root: None,
            chain: vec![],
            truncated: false,
            warnings: vec![format!(
                "symbol '{}' not found in repository",
                params.symbol_name
            )],
        });
    };

    // BFS traversal
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(root_ref.id.clone());

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((root_ref.id.clone(), 0));

    let mut chain: Vec<ChainNode> = vec![];
    let mut truncated = false;

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth >= params.max_depth {
            // Check if there are more edges from here
            let has_more = store
                .symbol_has_edges(
                    &current_id,
                    &params.direction,
                    params.min_confidence,
                    valid_at,
                )
                .await?;
            if has_more {
                truncated = true;
            }
            continue;
        }

        let edges = store
            .fetch_symbol_edges(
                &current_id,
                &params.direction,
                params.min_confidence,
                valid_at,
            )
            .await?;

        for edge in edges {
            let neighbor_id = &edge.neighbor_id;
            if visited.contains(neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id.clone());

            let sym = store.fetch_symbol_ref_by_id(neighbor_id, valid_at).await?;

            let node_name = sym
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| edge.to_name.clone());
            let node_file = sym.as_ref().map(|s| s.file.clone()).unwrap_or_default();
            let node_line = sym.as_ref().and_then(|s| s.line);
            chain.push(ChainNode {
                anchor: symbol_anchor(&node_file, node_line, &node_name),
                name: node_name,
                kind: sym.as_ref().map(|s| s.kind.clone()).unwrap_or_default(),
                file: node_file,
                line: node_line,
                via_edge: edge.edge_type.clone(),
                depth: depth + 1,
                confidence: edge.confidence,
            });

            queue.push_back((neighbor_id.clone(), depth + 1));
        }
    }

    Ok(TraceCallPathResult {
        root: Some(SymbolRef {
            anchor: symbol_anchor(&root_ref.file, root_ref.line, &root_ref.name),
            name: root_ref.name,
            kind: root_ref.kind,
            file: root_ref.file,
            line: root_ref.line,
        }),
        chain,
        truncated,
        warnings: vec![],
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn symbol_anchor(file: &str, line: Option<i64>, name: &str) -> String {
    match line {
        Some(l) => format!("{}:{} {}", file, l, name),
        None => format!("{} {}", file, name),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ingest_symbols::{run as ingest, IngestSymbolsParams};
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn trace_outbound_follows_call_chain() -> Result<(), Box<dyn std::error::Error>> {
        // A calls B calls C
        let td = tempdir()?;
        let repo = td.path().join("repo");
        fs::create_dir_all(repo.join("src"))?;

        // Write Rust file: fn c(){} fn b(){ c(); } fn a(){ b(); }
        fs::write(
            repo.join("src/lib.rs"),
            "fn c() {}\nfn b() { c(); }\nfn a() { b(); }\n",
        )?;

        let db_path = td.path().join("db.sqlite");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = crate::storage::SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = std::sync::Arc::new(store);

        ingest(
            &store,
            IngestSymbolsParams {
                repo_path: repo.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        let result = run(
            &store,
            TraceCallPathParams {
                repo_path: repo.to_string_lossy().to_string(),
                symbol_name: "a".to_string(),
                direction: TraceDirection::Outbound,
                max_depth: 4,
                min_confidence: 0.1,
                valid_at: None,
            },
        )
        .await?;

        assert!(result.root.is_some(), "root should be found");
        // b should appear at depth 1
        let b_node = result.chain.iter().find(|n| n.name == "b");
        assert!(b_node.is_some(), "b should appear in chain");
        assert_eq!(b_node.unwrap().depth, 1);
        // c should appear at depth 2
        let c_node = result.chain.iter().find(|n| n.name == "c");
        assert!(c_node.is_some(), "c should appear in chain");
        assert_eq!(c_node.unwrap().depth, 2);

        Ok(())
    }
}
