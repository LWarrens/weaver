//! Decisions, decision-code links, supersession edges, and the queries behind `find_decisions_for_code`.
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
    // Decisions
    // -----------------------------------------------------------------------

    pub async fn close_decisions_for_adr(
        &self,
        adr_doc_id: Uuid,
        valid_to: &str,
    ) -> Result<(), Error> {
        let id_str = adr_doc_id.to_string();
        sqlx::query(
            "UPDATE constraints SET valid_to = ? WHERE decision_id IN (SELECT id FROM decisions WHERE adr_id = ?)",
        )
        .bind(valid_to)
        .bind(&id_str)
        .execute(&self.pool)
        .await?;
        // Close the claims that hang off those decisions and constraints so a
        // superseded ADR does not leave open, unverifiable claims behind.
        sqlx::query(
            r#"UPDATE claims SET valid_to = ?
               WHERE valid_to IS NULL AND (
                 (subject_type = 'decision' AND subject_id IN
                    (SELECT id FROM decisions WHERE adr_id = ?))
                 OR (subject_type = 'constraint' AND subject_id IN
                    (SELECT id FROM constraints WHERE decision_id IN
                       (SELECT id FROM decisions WHERE adr_id = ?)))
               )"#,
        )
        .bind(valid_to)
        .bind(&id_str)
        .bind(&id_str)
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE decisions SET valid_to = ? WHERE adr_id = ?")
            .bind(valid_to)
            .bind(&id_str)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_decision(&self, d: &Decision) -> Result<(), Error> {
        let evidence_refs = serde_json::to_string(
            &d.evidence_refs
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();

        sqlx::query(
            r#"INSERT INTO decisions
               (id, title, adr_id, episode_id, text, source_uri, valid_from, valid_to,
                ingested_at, source_time, confidence, evidence_refs)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(d.id.to_string())
        .bind(&d.title)
        .bind(d.adr_id.map(|id| id.to_string()))
        .bind(d.episode_id.map(|id| id.to_string()))
        .bind(&d.text)
        .bind(&d.source_uri)
        .bind(&d.valid_from)
        .bind(&d.valid_to)
        .bind(&d.ingested_at)
        .bind(&d.source_time)
        .bind(d.confidence)
        .bind(&evidence_refs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_constraint(&self, c: &Constraint) -> Result<(), Error> {
        let evidence_refs = serde_json::to_string(
            &c.evidence_refs
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();

        sqlx::query(
            r#"INSERT INTO constraints
               (id, decision_id, text, source_uri, valid_from, valid_to,
                ingested_at, source_time, confidence, evidence_refs)
               VALUES (?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(c.id.to_string())
        .bind(c.decision_id.to_string())
        .bind(&c.text)
        .bind(&c.source_uri)
        .bind(&c.valid_from)
        .bind(&c.valid_to)
        .bind(&c.ingested_at)
        .bind(&c.source_time)
        .bind(c.confidence)
        .bind(&evidence_refs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_constraint_facts_for_adr(
        &self,
        adr_doc_id: Uuid,
    ) -> Result<Vec<(String, f64)>, Error> {
        let rows = sqlx::query(
            r#"SELECT c.text, c.confidence
               FROM constraints c
               JOIN decisions d ON c.decision_id = d.id
               WHERE d.adr_id = ?
               ORDER BY c.text, c.confidence"#,
        )
        .bind(adr_doc_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("text"), r.get("confidence")))
            .collect())
    }

    // -----------------------------------------------------------------------
    // Decision code links
    // -----------------------------------------------------------------------

    pub async fn insert_decision_code_link(&self, link: &DecisionCodeLink) -> Result<(), Error> {
        let link_type = match link.link_type {
            LinkType::Mentions => "mentions",
            LinkType::AppliesTo => "applies_to",
            LinkType::Modifies => "modifies",
        };
        let link_source = match link.link_source {
            LinkSource::AdrText => "adr_text",
            LinkSource::GitHistory => "git_history",
            LinkSource::Inferred => "inferred",
        };
        let evidence_refs = serde_json::to_string(
            &link
                .evidence_refs
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();

        sqlx::query(
            r#"INSERT OR IGNORE INTO decision_code_links
               (id, decision_id, file_path, symbol, link_type, link_source,
                confidence, valid_from, valid_to, ingested_at, evidence_refs)
               VALUES (?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(link.id.to_string())
        .bind(link.decision_id.to_string())
        .bind(&link.file_path)
        .bind(&link.symbol)
        .bind(link_type)
        .bind(link_source)
        .bind(link.confidence)
        .bind(&link.valid_from)
        .bind(&link.valid_to)
        .bind(&link.ingested_at)
        .bind(&evidence_refs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Supersession edges
    // -----------------------------------------------------------------------

    pub async fn upsert_supersession_edge(
        &self,
        superseder_id: Uuid,
        superseded_id: Uuid,
        valid_from: &str,
        ingested_at: &str,
    ) -> Result<(), Error> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT OR IGNORE INTO supersession_edges
               (id, superseder_id, superseded_id, valid_from, ingested_at, confidence, evidence_refs)
               VALUES (?,?,?,?,?,1.0,'[]')"#,
        )
        .bind(&id)
        .bind(superseder_id.to_string())
        .bind(superseded_id.to_string())
        .bind(valid_from)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[cfg(test)]
    pub async fn insert_test_temporal_edge(
        &self,
        edge_type: &str,
        source_decision_id: &str,
        target_decision_id: &str,
        valid_from: &str,
        ingested_at: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT INTO temporal_edges
               (id, edge_type, source_id, source_type, target_id, target_type,
                valid_from, valid_to, ingested_at, confidence, evidence_refs)
               VALUES (?, ?, ?, 'decision', ?, 'decision', ?, NULL, ?, 1.0, '[]')"#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(edge_type)
        .bind(source_decision_id)
        .bind(target_decision_id)
        .bind(valid_from)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

}

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Queries for find_decisions_for_code
    // -----------------------------------------------------------------------

    pub async fn find_decisions_for_file(
        &self,
        repo_id: Uuid,
        file_path: &str,
        valid_at: Option<&str>,
        temporal_mode: TemporalMode,
    ) -> Result<Vec<DecisionSummary>, Error> {
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");
        let (dcl_time_clause, decision_time_clause) = match temporal_mode {
            TemporalMode::Event => (
                "",
                r#"AND COALESCE(a.effective_from, e.occurred_at, d.valid_from) <= ?
                 AND (COALESCE(a.effective_to, d.valid_to) IS NULL OR COALESCE(a.effective_to, d.valid_to) > ?)"#,
            ),
            TemporalMode::Ingestion => (
                r#"AND dcl.valid_from <= ?
                 AND (dcl.valid_to IS NULL OR dcl.valid_to > ?)"#,
                r#"AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to > ?)"#,
            ),
        };

        let sql = format!(
            r#"SELECT d.id, d.text, d.valid_from, d.valid_to, d.confidence,
                      COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      d.episode_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      COALESCE(a.status, 'episode') AS status
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND dcl.file_path = ?
                 {}
                 {}"#,
            dcl_time_clause, decision_time_clause
        );

        let mut query = sqlx::query(&sql)
            .bind(repo_id.to_string())
            .bind(repo_id.to_string())
            .bind(file_path);
        if temporal_mode == TemporalMode::Ingestion {
            query = query.bind(at).bind(at);
        }
        let rows = query.bind(at).bind(at).fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| DecisionSummary {
                id: r.get("id"),
                adr_id: r.get("adr_id"),
                episode_id: r.get("episode_id"),
                title: r.get("title"),
                status: r.get("status"),
                text: r.get("text"),
                valid_from: r.get("valid_from"),
                valid_to: r.get("valid_to"),
                confidence: r.get("confidence"),
            })
            .collect())
    }

    /// Relative file paths with an open link to the given decision.
    pub async fn file_paths_linked_to_decision(
        &self,
        decision_id: &str,
    ) -> Result<Vec<String>, Error> {
        let rows = sqlx::query_scalar(
            r#"SELECT DISTINCT file_path FROM decision_code_links
               WHERE decision_id = ? AND valid_to IS NULL"#,
        )
        .bind(decision_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn find_constraints_for_decisions(
        &self,
        decision_ids: &[String],
    ) -> Result<Vec<ConstraintSummary>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders = decision_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, decision_id, text, confidence FROM constraints WHERE decision_id IN ({})",
            placeholders
        );

        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id.as_str());
        }

        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| ConstraintSummary {
                id: r.get("id"),
                decision_id: r.get("decision_id"),
                text: r.get("text"),
                confidence: r.get("confidence"),
            })
            .collect())
    }

}
