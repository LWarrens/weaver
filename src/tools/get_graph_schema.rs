use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::Error;
use crate::storage::sqlite::{SchemaColumn, SqliteStore};

const TABLES: &[&str] = &[
    "repositories",
    "adr_documents",
    "decisions",
    "constraints",
    "files",
    "symbols",
    "commits",
    "pull_requests",
    "episodes",
    "claims",
    "evidence_anchors",
    "evidence_verifications",
    "decision_code_links",
    "decision_git_links",
    "supersession_edges",
    "temporal_edges",
    "symbol_edges",
    "entity_nodes",
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetGraphSchemaParams {
    #[serde(default)]
    pub include_counts: bool,
}

#[derive(Debug, Serialize)]
pub struct GetGraphSchemaResult {
    pub tables: Vec<TableSchema>,
    pub node_tables: Vec<&'static str>,
    pub edge_tables: Vec<&'static str>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TableSchema {
    pub name: String,
    pub role: String,
    pub columns: Vec<SchemaColumn>,
    pub count: Option<i64>,
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: GetGraphSchemaParams,
) -> Result<GetGraphSchemaResult, Error> {
    let mut tables = Vec::new();

    for table in TABLES {
        let columns = store.table_columns(table).await?;
        let count = if params.include_counts {
            Some(store.table_count(table).await?)
        } else {
            None
        };
        tables.push(TableSchema {
            name: (*table).to_string(),
            role: table_role(table).to_string(),
            columns,
            count,
        });
    }

    Ok(GetGraphSchemaResult {
        tables,
        node_tables: vec![
            "repositories",
            "adr_documents",
            "decisions",
            "constraints",
            "files",
            "symbols",
            "commits",
            "pull_requests",
            "episodes",
        ],
        edge_tables: vec![
            "decision_code_links",
            "decision_git_links",
            "supersession_edges",
            "temporal_edges",
            "evidence_anchors",
        ],
        warnings: vec![
            "temporal_edges, commits, pull_requests, and decision_git_links may be empty until their ingestion tools are implemented".to_string(),
        ],
    })
}

fn table_role(table: &str) -> &'static str {
    match table {
        "repositories" => "tracked repository root",
        "adr_documents" => "parsed ADR source documents",
        "decisions" => "architectural decisions from ADRs or episodes",
        "constraints" => "decision constraints extracted from ADRs or supplied episodes",
        "files" => "tracked source files",
        "symbols" => "tree-sitter symbol index",
        "commits" => "git commit evidence table, currently schema-only",
        "pull_requests" => "pull request evidence table, currently schema-only",
        "episodes" => "raw architectural events and discussions",
        "claims" => "fine-grained verifiable assertions decomposed from decisions and constraints",
        "evidence_anchors" => "content-hashed citation spans backing each claim",
        "evidence_verifications" => "append-only per-anchor freshness checks against the working tree",
        "decision_code_links" => "decision to file or symbol links",
        "decision_git_links" => "decision to commit or PR links, currently schema-only",
        "supersession_edges" => "ADR supersession links",
        "temporal_edges" => "general typed graph edges",
        "symbol_edges" => "symbol call and import relationships",
        "entity_nodes" => "canonical entity identities resolved from ADR and episode mentions",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_schema_tables_and_counts() {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");
        let store = Arc::new(store);

        let result = run(
            &store,
            GetGraphSchemaParams {
                include_counts: true,
            },
        )
        .await
        .expect("schema");

        assert!(result.tables.iter().any(|table| table.name == "decisions"));
        assert!(result.tables.iter().all(|table| table.count.is_some()));
    }
}
