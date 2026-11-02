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
        // Close what ADR sync declared about those decisions: their code links
        // and the defines / imposes / mentions edges. Left open, they outlive
        // the decision and a full re-sync would never reproduce them (REQ-058).
        sqlx::query(
            r#"UPDATE decision_code_links SET valid_to = ?
               WHERE valid_to IS NULL
                 AND decision_id IN (SELECT id FROM decisions WHERE adr_id = ?)"#,
        )
        .bind(valid_to)
        .bind(&id_str)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"UPDATE temporal_edges SET valid_to = ?
               WHERE valid_to IS NULL
                 AND edge_type IN ('defines', 'imposes', 'mentions')
                 AND (target_id IN (SELECT id FROM decisions WHERE adr_id = ?)
                      OR source_id IN (SELECT id FROM decisions WHERE adr_id = ?))"#,
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
    /// `(id, text)` of a decision's open constraints.
    pub async fn open_constraints_for_decision(
        &self,
        decision_id: &str,
    ) -> Result<Vec<(String, String)>, Error> {
        let rows = sqlx::query_as(
            "SELECT id, text FROM constraints WHERE decision_id = ? AND valid_to IS NULL",
        )
        .bind(decision_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Fold a duplicate episode decision into `survivor` (entity resolution after
    /// the fact). Everything the loser carries that the survivor lacks moves over:
    /// the constraints in `move_constraint_ids` (with their `imposes` edges), code
    /// links to files the survivor is not already linked to, git links, and open
    /// temporal edges. A `supports` Episode -> Decision edge records the merge,
    /// as ingest-time dedup does. The loser is then soft-closed with its remaining
    /// constraints and claims, exactly as `retract` would.
    ///
    /// The re-homing runs in one transaction. If the process dies before the
    /// closing step the loser is still open, and re-running finds the same pair
    /// and finishes: every step is a no-op the second time.
    pub async fn merge_decision_into(
        &self,
        loser: Uuid,
        survivor: Uuid,
        episode_id: &str,
        similarity: f64,
        move_constraint_ids: &[String],
        now: &str,
    ) -> Result<(), Error> {
        let (loser_s, survivor_s) = (loser.to_string(), survivor.to_string());
        let mut tx = self.pool.begin().await?;

        let has_supports: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM temporal_edges
              WHERE edge_type = 'supports' AND source_id = ? AND target_id = ?
                AND valid_to IS NULL LIMIT 1",
        )
        .bind(episode_id)
        .bind(&survivor_s)
        .fetch_optional(&mut *tx)
        .await?;
        if has_supports.is_none() {
            sqlx::query(
                "INSERT INTO temporal_edges
                   (id, edge_type, source_id, source_type, target_id, target_type,
                    valid_from, valid_to, ingested_at, confidence, evidence_refs)
                 VALUES (?, 'supports', ?, 'episode', ?, 'decision', ?, NULL, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(episode_id)
            .bind(&survivor_s)
            .bind(now)
            .bind(now)
            .bind(similarity)
            .bind(serde_json::to_string(&[episode_id]).unwrap())
            .execute(&mut *tx)
            .await?;
        }

        for constraint_id in move_constraint_ids {
            sqlx::query(
                "UPDATE constraints SET decision_id = ?
                  WHERE id = ? AND decision_id = ? AND valid_to IS NULL",
            )
            .bind(&survivor_s)
            .bind(constraint_id)
            .bind(&loser_s)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE temporal_edges SET source_id = ?
                  WHERE edge_type = 'imposes' AND source_id = ? AND target_id = ?
                    AND valid_to IS NULL",
            )
            .bind(&survivor_s)
            .bind(&loser_s)
            .bind(constraint_id)
            .execute(&mut *tx)
            .await?;
        }

        // Code links: move those to files the survivor is not linked to, close the rest.
        sqlx::query(
            "UPDATE decision_code_links SET decision_id = ?1
              WHERE decision_id = ?2 AND valid_to IS NULL
                AND file_path NOT IN (SELECT file_path FROM decision_code_links
                                       WHERE decision_id = ?1 AND valid_to IS NULL)",
        )
        .bind(&survivor_s)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE decision_code_links SET valid_to = ? WHERE decision_id = ? AND valid_to IS NULL",
        )
        .bind(now)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;

        // Git links: `OR IGNORE` skips any the survivor already has (unique index).
        sqlx::query(
            "UPDATE OR IGNORE decision_git_links SET decision_id = ?
              WHERE decision_id = ? AND valid_to IS NULL",
        )
        .bind(&survivor_s)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE decision_git_links SET valid_to = ? WHERE decision_id = ? AND valid_to IS NULL",
        )
        .bind(now)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;

        // Open temporal edges touching the loser: re-point unless the survivor
        // already has the same edge, then close whatever is left (this includes
        // `imposes` edges to constraints that were duplicates and get closed).
        sqlx::query(
            "UPDATE temporal_edges SET target_id = ?1
              WHERE target_type = 'decision' AND target_id = ?2 AND valid_to IS NULL
                AND edge_type != 'imposes'
                AND NOT EXISTS (SELECT 1 FROM temporal_edges t
                                 WHERE t.edge_type = temporal_edges.edge_type
                                   AND t.source_id = temporal_edges.source_id
                                   AND t.target_id = ?1 AND t.valid_to IS NULL)",
        )
        .bind(&survivor_s)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE temporal_edges SET source_id = ?1
              WHERE source_type = 'decision' AND source_id = ?2 AND valid_to IS NULL
                AND edge_type != 'imposes'
                AND NOT EXISTS (SELECT 1 FROM temporal_edges t
                                 WHERE t.edge_type = temporal_edges.edge_type
                                   AND t.source_id = ?1
                                   AND t.target_id = temporal_edges.target_id
                                   AND t.valid_to IS NULL)",
        )
        .bind(&survivor_s)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE temporal_edges SET valid_to = ?1
              WHERE valid_to IS NULL
                AND ((target_type = 'decision' AND target_id = ?2)
                  OR (source_type = 'decision' AND source_id = ?2))",
        )
        .bind(now)
        .bind(&loser_s)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        self.retract_decision(loser, now).await?;
        Ok(())
    }

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
