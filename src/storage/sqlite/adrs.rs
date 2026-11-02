//! ADR document rows.
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
    // ADR documents
    // -----------------------------------------------------------------------

    /// Natural-key fingerprints of the ADR lane's declared facts, one string per
    /// row, for the integrity oracle (REQ-058). No UUID or timestamp appears in a
    /// key, so a live index and a from-scratch rebuild compare equal when they
    /// declare the same facts. Duplicate strings are meaningful: callers compare
    /// multisets.
    ///
    /// Only the latest document version per `adr_id` is projected (a full
    /// re-sync has no history). Links and edges are deliberately not filtered by
    /// their decision's validity: an open row hanging off a closed decision is
    /// exactly the stale state the oracle exists to surface.
    ///
    /// `mentions` edges are not projected: the ADR parser never fills service or
    /// module mentions, so ADR sync cannot currently create them.
    pub async fn adr_declared_facts(&self, repo_id: Uuid) -> Result<Vec<String>, Error> {
        let rows: Vec<String> = sqlx::query_scalar(
            r#"WITH latest AS (
                   SELECT a.id, a.adr_id, a.title, a.status, a.valid_to
                   FROM adr_documents a
                   WHERE a.repo_id = ?1
                     AND a.id = (SELECT b.id FROM adr_documents b
                                 WHERE b.repo_id = a.repo_id AND b.adr_id = a.adr_id
                                 ORDER BY b.ingested_at DESC, b.valid_from DESC LIMIT 1)
               )
               SELECT 'adr|' || adr_id || '|' || status || '|'
                      || CASE WHEN valid_to IS NULL THEN 'open' ELSE 'closed' END
                      || '|' || title
               FROM latest
               UNION ALL
               SELECT 'supersedes|' || n.adr_id || '|' || o.adr_id
               FROM supersession_edges s
               JOIN latest n ON n.id = s.superseder_id
               JOIN latest o ON o.id = s.superseded_id
               UNION ALL
               SELECT 'link|' || a.adr_id || '|' || dcl.file_path || '|'
                      || COALESCE(dcl.symbol, '') || '|' || dcl.link_type
                      || '|' || printf('%.2f', dcl.confidence)
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               JOIN adr_documents a ON a.id = d.adr_id
               WHERE a.repo_id = ?1 AND dcl.valid_to IS NULL AND dcl.link_source = 'adr_text'
               UNION ALL
               SELECT 'defines|' || a.adr_id || '|' || printf('%.2f', te.confidence)
               FROM temporal_edges te
               JOIN decisions d ON d.id = te.target_id
               JOIN adr_documents a ON a.id = d.adr_id
               WHERE te.edge_type = 'defines' AND te.valid_to IS NULL AND a.repo_id = ?1
               UNION ALL
               SELECT 'imposes|' || a.adr_id || '|' || c.text || '|' || printf('%.2f', te.confidence)
               FROM temporal_edges te
               JOIN decisions d ON d.id = te.source_id
               JOIN constraints c ON c.id = te.target_id
               JOIN adr_documents a ON a.id = d.adr_id
               WHERE te.edge_type = 'imposes' AND te.valid_to IS NULL AND a.repo_id = ?1"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn find_current_adr(
        &self,
        repo_id: Uuid,
        source_uri: &str,
    ) -> Result<Option<AdrDocument>, Error> {
        let row = sqlx::query(
            r#"SELECT id, repo_id, adr_id, title, status, date, context, decision,
                      consequences, supersedes, superseded_by, file_mentions,
                      service_mentions, module_mentions, source_uri,
                      effective_from, effective_to, valid_from, valid_to,
                      ingested_at, source_time, confidence
               FROM adr_documents
               WHERE repo_id = ? AND source_uri = ? AND valid_to IS NULL
               LIMIT 1"#,
        )
        .bind(repo_id.to_string())
        .bind(source_uri)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_adr(&r)).transpose()
    }

    pub async fn close_adr(&self, adr_id: Uuid, valid_to: &str) -> Result<(), Error> {
        sqlx::query("UPDATE adr_documents SET valid_to = ? WHERE id = ?")
            .bind(valid_to)
            .bind(adr_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_adr(&self, doc: &AdrDocument) -> Result<(), Error> {
        let supersedes = serde_json::to_string(&doc.supersedes).unwrap();
        let file_mentions = serde_json::to_string(&doc.file_mentions).unwrap();
        let service_mentions = serde_json::to_string(&doc.service_mentions).unwrap();
        let module_mentions = serde_json::to_string(&doc.module_mentions).unwrap();

        sqlx::query(
            r#"INSERT INTO adr_documents
               (id, repo_id, adr_id, title, status, date, context, decision,
                consequences, supersedes, superseded_by, file_mentions,
                service_mentions, module_mentions, source_uri,
                effective_from, effective_to, valid_from, valid_to,
                ingested_at, source_time, confidence)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(doc.id.to_string())
        .bind(doc.repo_id.to_string())
        .bind(&doc.adr_id)
        .bind(&doc.title)
        .bind(doc.status.as_str())
        .bind(&doc.date)
        .bind(&doc.context)
        .bind(&doc.decision)
        .bind(&doc.consequences)
        .bind(&supersedes)
        .bind(&doc.superseded_by)
        .bind(&file_mentions)
        .bind(&service_mentions)
        .bind(&module_mentions)
        .bind(&doc.source_uri)
        .bind(&doc.effective_from)
        .bind(&doc.effective_to)
        .bind(&doc.valid_from)
        .bind(&doc.valid_to)
        .bind(&doc.ingested_at)
        .bind(&doc.source_time)
        .bind(doc.confidence)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Supersession pairs joined with the superseded side's current state:
    /// (superseder_adr_id, superseded_adr_id, superseded_status, superseded_valid_to).
    pub async fn supersession_pairs_with_status(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String, Option<String>)>, Error> {
        let rows = sqlx::query(
            r#"SELECT a1.adr_id AS superseder, a2.adr_id AS superseded,
                      a2.status AS superseded_status, a2.valid_to AS superseded_valid_to
               FROM supersession_edges se
               JOIN adr_documents a1 ON a1.id = se.superseder_id
               JOIN adr_documents a2 ON a2.id = se.superseded_id
               WHERE a1.repo_id = ?"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("superseder"),
                    r.get("superseded"),
                    r.get("superseded_status"),
                    r.get("superseded_valid_to"),
                )
            })
            .collect())
    }

    pub async fn list_current_adrs(&self, repo_id: Uuid) -> Result<Vec<AdrDocument>, Error> {
        let rows = sqlx::query(
            r#"SELECT id, repo_id, adr_id, title, status, date, context, decision,
                      consequences, supersedes, superseded_by, file_mentions,
                      service_mentions, module_mentions, source_uri,
                      effective_from, effective_to, valid_from, valid_to,
                      ingested_at, source_time, confidence
               FROM adr_documents
               WHERE repo_id = ? AND valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(|r| row_to_adr(r)).collect()
    }
}
