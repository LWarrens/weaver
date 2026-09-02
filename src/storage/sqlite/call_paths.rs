//! Call-path traversal helpers for `trace_call_path`.
//!
//! Split out of the original monolithic `sqlite.rs`; all methods extend
//! [`SqliteStore`].

#![allow(unused_imports)]

use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use uuid::Uuid;

use crate::domain::entities::{
    AdrDocument, AdrStatus, Constraint, ConstraintSummary, Decision, DecisionCodeLink,
    DecisionSummary, EntityNode, LinkSource, LinkType, Repository, TemporalMode,
};
use crate::error::Error;

use super::*;

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Call-path traversal helpers
    // -----------------------------------------------------------------------

    /// Find a symbol by exact name or suffix match within a repository.
    pub async fn find_symbol_ref_by_name(
        &self,
        repo_id: Uuid,
        name: &str,
        valid_at: &str,
    ) -> Result<Option<SymbolRow>, Error> {
        let repo_id_str = repo_id.to_string();
        // Exact match first
        let row = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file, s.line
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.name = ?
                 AND s.valid_from <= ? AND (s.valid_to IS NULL OR s.valid_to > ?)
               LIMIT 1"#,
        )
        .bind(&repo_id_str)
        .bind(name)
        .bind(valid_at)
        .bind(valid_at)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            return Ok(Some(SymbolRow {
                id: r.get("id"),
                name: r.get("name"),
                kind: r.get("kind"),
                file: r.get("file"),
                line: r.get("line"),
            }));
        }

        // Suffix match: name ends with "::<name>" or "::<name>"
        let suffix = format!("%::{}", name);
        let row = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file, s.line
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.name LIKE ?
                 AND s.valid_from <= ? AND (s.valid_to IS NULL OR s.valid_to > ?)
               LIMIT 1"#,
        )
        .bind(&repo_id_str)
        .bind(&suffix)
        .bind(valid_at)
        .bind(valid_at)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| SymbolRow {
            id: r.get("id"),
            name: r.get("name"),
            kind: r.get("kind"),
            file: r.get("file"),
            line: r.get("line"),
        }))
    }

    /// Fetch a symbol's details by its ID.
    pub async fn fetch_symbol_ref_by_id(
        &self,
        symbol_id: &str,
        valid_at: &str,
    ) -> Result<Option<SymbolRow>, Error> {
        let row = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file, s.line
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE s.id = ?
                 AND s.valid_from <= ? AND (s.valid_to IS NULL OR s.valid_to > ?)"#,
        )
        .bind(symbol_id)
        .bind(valid_at)
        .bind(valid_at)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| SymbolRow {
            id: r.get("id"),
            name: r.get("name"),
            kind: r.get("kind"),
            file: r.get("file"),
            line: r.get("line"),
        }))
    }

    /// Fetch edges in the requested direction from a symbol node.
    pub async fn fetch_symbol_edges(
        &self,
        symbol_id: &str,
        direction: &crate::tools::trace_call_path::TraceDirection,
        min_confidence: f64,
        valid_at: &str,
    ) -> Result<Vec<EdgeRow>, Error> {
        use crate::tools::trace_call_path::TraceDirection;

        let include_outbound = matches!(direction, TraceDirection::Outbound | TraceDirection::Both);
        let include_inbound = matches!(direction, TraceDirection::Inbound | TraceDirection::Both);

        let mut rows = Vec::new();

        if include_outbound {
            let outbound = sqlx::query(
                r#"SELECT se.to_id AS neighbor_id,
                          COALESCE(se.to_name, '') AS to_name,
                          se.edge_type, se.confidence
                   FROM symbol_edges se
                   WHERE se.from_id = ?
                     AND se.confidence >= ?
                     AND se.valid_from <= ?
                     AND (se.valid_to IS NULL OR se.valid_to > ?)
                     AND se.to_id IS NOT NULL"#,
            )
            .bind(symbol_id)
            .bind(min_confidence)
            .bind(valid_at)
            .bind(valid_at)
            .fetch_all(&self.pool)
            .await?;

            for r in outbound {
                rows.push(EdgeRow {
                    neighbor_id: r.get("neighbor_id"),
                    to_name: r.get("to_name"),
                    edge_type: r.get("edge_type"),
                    confidence: r.get("confidence"),
                });
            }
        }

        if include_inbound {
            let inbound = sqlx::query(
                r#"SELECT se.from_id AS neighbor_id,
                          COALESCE(se.to_name, '') AS to_name,
                          se.edge_type, se.confidence
                   FROM symbol_edges se
                   WHERE se.to_id = ?
                     AND se.confidence >= ?
                     AND se.valid_from <= ?
                     AND (se.valid_to IS NULL OR se.valid_to > ?)"#,
            )
            .bind(symbol_id)
            .bind(min_confidence)
            .bind(valid_at)
            .bind(valid_at)
            .fetch_all(&self.pool)
            .await?;

            for r in inbound {
                rows.push(EdgeRow {
                    neighbor_id: r.get("neighbor_id"),
                    to_name: r.get("to_name"),
                    edge_type: r.get("edge_type"),
                    confidence: r.get("confidence"),
                });
            }
        }

        Ok(rows)
    }

    /// Check if a symbol has any traversable edges (used for `truncated` detection).
    pub async fn symbol_has_edges(
        &self,
        symbol_id: &str,
        direction: &crate::tools::trace_call_path::TraceDirection,
        min_confidence: f64,
        valid_at: &str,
    ) -> Result<bool, Error> {
        use crate::tools::trace_call_path::TraceDirection;

        let (col, id_col) = match direction {
            TraceDirection::Outbound | TraceDirection::Both => ("from_id", "to_id"),
            TraceDirection::Inbound => ("to_id", "from_id"),
        };

        let sql = format!(
            r#"SELECT 1 FROM symbol_edges
               WHERE {} = ? AND {} IS NOT NULL
                 AND confidence >= ?
                 AND valid_from <= ?
                 AND (valid_to IS NULL OR valid_to > ?)
               LIMIT 1"#,
            col, id_col
        );

        let row = sqlx::query(&sql)
            .bind(symbol_id)
            .bind(min_confidence)
            .bind(valid_at)
            .bind(valid_at)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.is_some())
    }

    pub async fn find_adr_by_adr_id(
        &self,
        repo_id: Uuid,
        adr_id: &str,
    ) -> Result<Option<AdrDocument>, Error> {
        let row = sqlx::query(
            r#"SELECT id, repo_id, adr_id, title, status, date, context, decision,
                      consequences, supersedes, superseded_by, file_mentions,
                      service_mentions, module_mentions, source_uri,
                      effective_from, effective_to, valid_from, valid_to,
                      ingested_at, source_time, confidence
               FROM adr_documents
               WHERE repo_id = ? AND adr_id = ? AND valid_to IS NULL
               LIMIT 1"#,
        )
        .bind(repo_id.to_string())
        .bind(adr_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_adr(&r)).transpose()
    }

}
