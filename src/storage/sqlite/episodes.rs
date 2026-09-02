//! Episodes and temporal edges.
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
    // Episodes (Phase 3)
    // -----------------------------------------------------------------------

    /// Insert an episode record. `source_uri` may be `None`.
    pub async fn insert_episode(
        &self,
        id: &Uuid,
        repo_id: Uuid,
        source: &str,
        source_uri: Option<&str>,
        content: &str,
        occurred_at: &str,
        ingested_at: &str,
    ) -> Result<(), Error> {
        let id_str = id.to_string();
        sqlx::query(
            r#"INSERT INTO episodes (id, repo_id, source, source_uri, content, occurred_at, ingested_at, confidence, evidence_refs)
               VALUES (?,?,?,?,?,?,?,1.0,'[]')"#,
        )
        .bind(&id_str)
        .bind(repo_id.to_string())
        .bind(source)
        .bind(source_uri)
        .bind(content)
        .bind(occurred_at)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn insert_temporal_edge(&self, edge: &TemporalEdge) -> Result<(), Error> {
        let evidence_refs = serde_json::to_string(
            &edge
                .evidence_refs
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();

        sqlx::query(
            r#"INSERT INTO temporal_edges
               (id, edge_type, source_id, source_type, target_id, target_type,
                valid_from, valid_to, ingested_at, confidence, evidence_refs)
               VALUES (?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(edge.id.to_string())
        .bind(&edge.edge_type)
        .bind(&edge.source_id)
        .bind(&edge.source_type)
        .bind(&edge.target_id)
        .bind(&edge.target_type)
        .bind(&edge.valid_from)
        .bind(&edge.valid_to)
        .bind(&edge.ingested_at)
        .bind(edge.confidence)
        .bind(&evidence_refs)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Insert a temporal edge only if no open edge with the same
    /// (edge_type, source_id, target_id) already exists. Keeps re-runs of
    /// ingest tools (e.g. `sync_commits_from_git`) idempotent.
    /// Returns `true` if the edge was inserted.
    pub async fn insert_temporal_edge_if_absent(&self, edge: &TemporalEdge) -> Result<bool, Error> {
        let existing: Option<(String,)> = sqlx::query_as(
            r#"SELECT id FROM temporal_edges
               WHERE edge_type = ? AND source_id = ? AND target_id = ?
                 AND valid_to IS NULL
               LIMIT 1"#,
        )
        .bind(&edge.edge_type)
        .bind(&edge.source_id)
        .bind(&edge.target_id)
        .fetch_optional(&self.pool)
        .await?;

        if existing.is_some() {
            return Ok(false);
        }
        self.insert_temporal_edge(edge).await?;
        Ok(true)
    }

    /// Open `conflicts_with` edges where either endpoint is one of the given
    /// decision ids.
    pub async fn open_conflict_edges_for_decisions(
        &self,
        decision_ids: &[String],
    ) -> Result<Vec<TemporalEdge>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }
        let clause = decision_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            r#"SELECT * FROM temporal_edges
               WHERE edge_type = 'conflicts_with' AND valid_to IS NULL
                 AND (source_id IN ({clause}) OR target_id IN ({clause}))"#
        );
        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id);
        }
        for id in decision_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_temporal_edge).collect()
    }

    /// Count open temporal edges of a given type. Used by diagnostics and
    /// integration tests.
    pub async fn count_open_temporal_edges_of_type(&self, edge_type: &str) -> Result<i64, Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM temporal_edges WHERE edge_type = ? AND valid_to IS NULL",
        )
        .bind(edge_type)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    #[cfg(test)]
    pub async fn temporal_edges_for_evidence(
        &self,
        episode_id: Uuid,
    ) -> Result<Vec<TemporalEdge>, Error> {
        let rows = sqlx::query(
            r#"SELECT id, edge_type, source_id, source_type, target_id, target_type,
                      valid_from, valid_to, ingested_at, confidence, evidence_refs
               FROM temporal_edges
               WHERE evidence_refs LIKE ?
               ORDER BY edge_type, source_id, target_id"#,
        )
        .bind(format!("%{}%", episode_id))
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_temporal_edge).collect()
    }

    /// Fetch all valid temporal edges from a given source entity.
    pub async fn fetch_impact_edges(
        &self,
        source_id: &str,
        edge_types: &[&str],
        valid_at: &str,
    ) -> Result<Vec<TemporalEdge>, Error> {
        if edge_types.is_empty() {
            return Ok(vec![]);
        }

        // Build a dynamic IN clause — safe because edge_types come from our
        // own code (validated enum values), not from user input.
        let placeholders = edge_types
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"SELECT id, edge_type, source_id, source_type, target_id, target_type,
                      valid_from, valid_to, ingested_at, confidence, evidence_refs
               FROM temporal_edges
               WHERE source_id = ?
                 AND edge_type IN ({})
                 AND valid_from <= ?
                 AND (valid_to IS NULL OR valid_to > ?)"#,
            placeholders
        );

        let mut q = sqlx::query(&sql).bind(source_id);
        for et in edge_types {
            q = q.bind(*et);
        }
        let rows = q
            .bind(valid_at)
            .bind(valid_at)
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(row_to_temporal_edge).collect()
    }

}
