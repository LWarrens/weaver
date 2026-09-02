use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use uuid::Uuid;

use crate::domain::entities::{AdrDocument, AdrStatus, DecisionSummary};
use crate::error::Error;

const QUERY_EDGE_TYPES: &[&str] = &["applies_to", "conflicts_with", "supports", "depends_on"];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SymbolEdge {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub from_id: Uuid,
    pub to_id: Option<Uuid>,
    pub to_name: Option<String>,
    pub edge_type: String,
    pub confidence: f64,
    pub valid_from: String,
}

// ---------------------------------------------------------------------------
// trace_symbol_history types
// ---------------------------------------------------------------------------

/// One appearance of a symbol name in the ingested index.
#[derive(Debug, Clone)]
pub struct SymbolSpan {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

// ---------------------------------------------------------------------------
// Call-path traversal types
// ---------------------------------------------------------------------------

/// Resolved symbol reference used in call-path traversal.
#[derive(Debug)]
pub struct SymbolRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: Option<i64>,
}

/// An edge returned during BFS traversal.
#[derive(Debug)]
pub struct EdgeRow {
    pub neighbor_id: String,
    pub to_name: String,
    pub edge_type: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphNodeRow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphEdgeRow {
    pub id: String,
    pub source: String,
    pub target: String,
    pub edge_type: String,
    pub confidence: f64,
    pub cross_file: bool,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaColumn {
    pub name: String,
    pub column_type: String,
    pub not_null: bool,
    pub primary_key: bool,
}

#[derive(Debug, Clone)]
pub struct TemporalEdge {
    pub id: Uuid,
    pub edge_type: String,
    pub source_id: String,
    pub source_type: String,
    pub target_id: String,
    pub target_type: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub confidence: f64,
    pub evidence_refs: Vec<Uuid>,
}

/// Embedding and ingestion coverage data returned by `index_status_counts`.
#[derive(Debug, Clone)]
pub struct IndexStatusCounts {
    pub adrs_total: i64,
    pub adrs_last_ingested_at: Option<String>,
    pub decisions_total: i64,
    pub decisions_embedded: i64,
    pub decisions_last_ingested_at: Option<String>,
    pub constraints_total: i64,
    pub constraints_embedded: i64,
    pub constraints_last_ingested_at: Option<String>,
    pub episodes_total: i64,
    pub episodes_embedded: i64,
    pub episodes_last_ingested_at: Option<String>,
    pub commits_total: i64,
    pub commits_embedded: i64,
    pub commits_last_ingested_at: Option<String>,
    pub symbols_total: i64,
    pub symbols_embedded: i64,
    pub symbols_last_ingested_at: Option<String>,
    pub files_total: i64,
    pub files_last_ingested_at: Option<String>,
}

fn sqlite_limit(limit: Option<usize>) -> i64 {
    limit
        .map(|value| value.min(i64::MAX as usize) as i64)
        .unwrap_or(-1)
}

fn within_budget(count: usize, limit: Option<usize>) -> bool {
    limit.map(|value| count < value).unwrap_or(true)
}


// ---------------------------------------------------------------------------
// Submodules - one file per query family; every file extends `SqliteStore`
// with an inherent impl block.
// ---------------------------------------------------------------------------

mod adrs;
mod call_paths;
mod claims;
mod decisions;
mod embeddings;
mod entities;
mod episodes;
mod git;
mod graph;
mod repositories;
mod search;
mod symbols;
mod tool_support;

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Connect to the SQLite database and enable WAL mode.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // single-writer model
            .connect(url)
            .await?;

        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await?;

        Ok(SqliteStore { pool })
    }

    /// Run all pending migrations from the `migrations/` directory.
    pub async fn run_migrations(&self) -> Result<(), Error> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| Error::Migration(e.to_string()))?;
        Ok(())
    }

}

// ---------------------------------------------------------------------------
// Row conversion helper
// ---------------------------------------------------------------------------

fn row_to_adr(r: &sqlx::sqlite::SqliteRow) -> Result<AdrDocument, Error> {
    let id_str: String = r.get("id");
    let repo_id_str: String = r.get("repo_id");
    let supersedes_json: String = r.get("supersedes");
    let file_mentions_json: String = r.get("file_mentions");
    let service_mentions_json: String = r.get("service_mentions");
    let module_mentions_json: String = r.get("module_mentions");

    Ok(AdrDocument {
        id: Uuid::parse_str(&id_str).map_err(|e| Error::Parse(e.to_string()))?,
        repo_id: Uuid::parse_str(&repo_id_str).map_err(|e| Error::Parse(e.to_string()))?,
        adr_id: r.get("adr_id"),
        title: r.get("title"),
        status: AdrStatus::from_str(r.get::<&str, _>("status")),
        date: r.get("date"),
        context: r.get("context"),
        decision: r.get("decision"),
        consequences: r.get("consequences"),
        supersedes: serde_json::from_str(&supersedes_json).unwrap_or_default(),
        superseded_by: r.get("superseded_by"),
        file_mentions: serde_json::from_str(&file_mentions_json).unwrap_or_default(),
        service_mentions: serde_json::from_str(&service_mentions_json).unwrap_or_default(),
        module_mentions: serde_json::from_str(&module_mentions_json).unwrap_or_default(),
        source_uri: r.get("source_uri"),
        effective_from: r.get("effective_from"),
        effective_to: r.get("effective_to"),
        valid_from: r.get("valid_from"),
        valid_to: r.get("valid_to"),
        ingested_at: r.get("ingested_at"),
        source_time: r.get("source_time"),
        confidence: r.get("confidence"),
    })
}

fn row_to_temporal_edge(r: &sqlx::sqlite::SqliteRow) -> Result<TemporalEdge, Error> {
    let evidence_refs_json: String = r.get("evidence_refs");
    let evidence_refs = serde_json::from_str::<Vec<String>>(&evidence_refs_json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| Uuid::parse_str(&id).ok())
        .collect();

    Ok(TemporalEdge {
        id: Uuid::parse_str(r.get::<&str, _>("id")).map_err(|e| Error::Parse(e.to_string()))?,
        edge_type: r.get("edge_type"),
        source_id: r.get("source_id"),
        source_type: r.get("source_type"),
        target_id: r.get("target_id"),
        target_type: r.get("target_type"),
        valid_from: r.get("valid_from"),
        valid_to: r.get("valid_to"),
        ingested_at: r.get("ingested_at"),
        confidence: r.get("confidence"),
        evidence_refs,
    })
}

// ---------------------------------------------------------------------------
// Communities
// ---------------------------------------------------------------------------

fn is_known_table(table: &str) -> bool {
    matches!(
        table,
        "repositories"
            | "adr_documents"
            | "decisions"
            | "constraints"
            | "files"
            | "symbols"
            | "symbol_edges"
            | "commits"
            | "pull_requests"
            | "episodes"
            | "claims"
            | "evidence_anchors"
            | "evidence_verifications"
            | "index_lanes"
            | "freshness_manifests"
            | "decision_code_links"
            | "decision_git_links"
            | "supersession_edges"
            | "temporal_edges"
            | "entity_nodes"
            | "communities"
            | "community_members"
            | "routes"
    )
}

fn term_frequency_score(decision: &DecisionSummary, terms: &[String]) -> usize {
    let haystack = format!("{} {}", decision.title, decision.text).to_lowercase();
    terms
        .iter()
        .map(|term| haystack.matches(term).count())
        .sum()
}

async fn count_query(
    pool: &SqlitePool,
    query: &str,
    repo_id: Uuid,
    bind_repo_twice: bool,
) -> Result<i64, Error> {
    let mut query = sqlx::query(query).bind(repo_id.to_string());
    if bind_repo_twice {
        query = query.bind(repo_id.to_string());
    }
    let row = query.fetch_one(pool).await?;
    Ok(row.get("count"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
