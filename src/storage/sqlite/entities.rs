//! Entity nodes (services, modules) resolved from ADRs and episodes.
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
    // Entity Nodes
    // -----------------------------------------------------------------------

    /// Upsert an entity node by canonical name. Returns the existing node if already active.
    pub async fn upsert_entity_node(
        &self,
        repo_id: Uuid,
        canonical_name: &str,
        entity_type: Option<&str>,
        confidence: f64,
        ingested_at: &str,
    ) -> Result<EntityNode, Error> {
        // Check for existing active node
        let existing = sqlx::query(
            "SELECT id FROM entity_nodes WHERE repo_id = ? AND canonical_name = ? AND valid_to IS NULL LIMIT 1"
        )
        .bind(repo_id.to_string())
        .bind(canonical_name)
        .fetch_optional(&self.pool)
        .await?;

        let entity_id = if let Some(row) = existing {
            Uuid::parse_str(&row.get::<String, _>("id"))
                .map_err(|_| crate::error::Error::Other(anyhow::anyhow!("invalid UUID in entity_nodes")))?
        } else {
            let id = Uuid::new_v4();
            sqlx::query(
                r#"INSERT INTO entity_nodes
                   (id, repo_id, canonical_name, entity_type, confidence, valid_from, ingested_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(id.to_string())
            .bind(repo_id.to_string())
            .bind(canonical_name)
            .bind(entity_type)
            .bind(confidence)
            .bind(ingested_at)
            .bind(ingested_at)
            .execute(&self.pool)
            .await?;
            id
        };

        let row = sqlx::query(
            "SELECT id, repo_id, canonical_name, entity_type, confidence, valid_from, valid_to, ingested_at, evidence_refs FROM entity_nodes WHERE id = ?"
        )
        .bind(entity_id.to_string())
        .fetch_one(&self.pool)
        .await?;

        let evidence_refs_json: String = row.get("evidence_refs");
        let evidence_refs: Vec<Uuid> = serde_json::from_str(&evidence_refs_json).unwrap_or_default();

        Ok(EntityNode {
            id: entity_id,
            repo_id,
            canonical_name: row.get("canonical_name"),
            entity_type: row.get("entity_type"),
            confidence: row.get("confidence"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
            ingested_at: row.get("ingested_at"),
            evidence_refs,
        })
    }

    /// Current entity nodes matching a name, case-insensitively. When
    /// `entity_type` is given, untyped nodes also match — episode-created
    /// entities carry no type.
    pub async fn find_entity_nodes_by_name(
        &self,
        repo_id: Uuid,
        name: &str,
        entity_type: Option<&str>,
    ) -> Result<Vec<EntityNode>, Error> {
        let rows = sqlx::query(
            r#"SELECT id, repo_id, canonical_name, entity_type, confidence,
                      valid_from, valid_to, ingested_at, evidence_refs
               FROM entity_nodes
               WHERE repo_id = ? AND LOWER(canonical_name) = LOWER(?)
                 AND valid_to IS NULL
                 AND (? IS NULL OR entity_type = ? OR entity_type IS NULL)"#,
        )
        .bind(repo_id.to_string())
        .bind(name)
        .bind(entity_type)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let evidence_refs_json: String = row.get("evidence_refs");
                let evidence_refs: Vec<Uuid> =
                    serde_json::from_str(&evidence_refs_json).unwrap_or_default();
                Ok(EntityNode {
                    id: Uuid::parse_str(&row.get::<String, _>("id")).map_err(|_| {
                        crate::error::Error::Other(anyhow::anyhow!("invalid UUID in entity_nodes"))
                    })?,
                    repo_id,
                    canonical_name: row.get("canonical_name"),
                    entity_type: row.get("entity_type"),
                    confidence: row.get("confidence"),
                    valid_from: row.get("valid_from"),
                    valid_to: row.get("valid_to"),
                    ingested_at: row.get("ingested_at"),
                    evidence_refs,
                })
            })
            .collect()
    }

    /// Decision ids with an open `mentions` edge to any of the given entities,
    /// valid at `valid_at` (defaults to open-ended).
    pub async fn decision_ids_mentioning_entities(
        &self,
        entity_ids: &[String],
        valid_at: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        if entity_ids.is_empty() {
            return Ok(vec![]);
        }
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");
        let clause = entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            r#"SELECT DISTINCT source_id FROM temporal_edges
               WHERE edge_type = 'mentions'
                 AND source_type = 'decision' AND target_type = 'entity'
                 AND target_id IN ({clause})
                 AND valid_from <= ? AND (valid_to IS NULL OR valid_to > ?)"#
        );
        let mut q = sqlx::query_scalar(&sql);
        for id in entity_ids {
            q = q.bind(id);
        }
        q = q.bind(at).bind(at);
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// Decision ids whose open code links live under a path segment equal to
    /// the given module name (e.g. "storage" matches "src/storage/sqlite.rs"
    /// and "storage/mod.rs"). Case-insensitive.
    pub async fn decision_ids_linked_under_path_segment(
        &self,
        segment: &str,
    ) -> Result<Vec<String>, Error> {
        let rows = sqlx::query_scalar(
            r#"SELECT DISTINCT decision_id FROM decision_code_links
               WHERE valid_to IS NULL
                 AND INSTR(LOWER('/' || REPLACE(file_path, '\', '/') || '/'),
                           LOWER('/' || ? || '/')) > 0"#,
        )
        .bind(segment)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    #[allow(dead_code)]
    pub async fn list_current_entity_nodes(&self, repo_id: Uuid) -> Result<Vec<EntityNode>, Error> {
        let rows = sqlx::query(
            "SELECT id, repo_id, canonical_name, entity_type, confidence, valid_from, valid_to, ingested_at, evidence_refs FROM entity_nodes WHERE repo_id = ? AND valid_to IS NULL"
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let evidence_refs_json: String = row.get("evidence_refs");
                let evidence_refs: Vec<Uuid> = serde_json::from_str(&evidence_refs_json).unwrap_or_default();
                Ok(EntityNode {
                    id: Uuid::parse_str(&row.get::<String, _>("id"))
                        .map_err(|_| crate::error::Error::Other(anyhow::anyhow!("invalid UUID")))?,
                    repo_id,
                    canonical_name: row.get("canonical_name"),
                    entity_type: row.get("entity_type"),
                    confidence: row.get("confidence"),
                    valid_from: row.get("valid_from"),
                    valid_to: row.get("valid_to"),
                    ingested_at: row.get("ingested_at"),
                    evidence_refs,
                })
            })
            .collect()
    }

}
