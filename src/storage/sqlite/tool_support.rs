//! Per-tool query helpers: retraction, propose_links, find_stale_decisions, trace_symbol_history, freshness, and focused_file_brief.
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
    // Retraction
    // -----------------------------------------------------------------------

    /// Soft-delete a decision and cascade to its active constraints.
    /// Returns `(decisions_closed, constraints_closed)`.
    pub async fn retract_decision(
        &self,
        id: Uuid,
        valid_to: &str,
    ) -> Result<(i64, i64), Error> {
        let id_str = id.to_string();

        let d = sqlx::query(
            "UPDATE decisions SET valid_to = ? WHERE id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(&id_str)
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        let c = sqlx::query(
            "UPDATE constraints SET valid_to = ? WHERE decision_id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(&id_str)
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        self.close_claims_for_decision_subtree(&id_str, valid_to).await?;

        Ok((d, c))
    }

    /// Soft-delete a single constraint. Returns the number of rows closed (0 or 1).
    pub async fn retract_constraint(&self, id: Uuid, valid_to: &str) -> Result<i64, Error> {
        let c = sqlx::query(
            "UPDATE constraints SET valid_to = ? WHERE id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        self.close_claims_for_subject("constraint", id, valid_to).await?;

        Ok(c)
    }

    /// Close every open claim (decision + its constraints) under a decision id.
    async fn close_claims_for_decision_subtree(
        &self,
        decision_id: &str,
        valid_to: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"UPDATE claims SET valid_to = ?
               WHERE valid_to IS NULL AND (
                 (subject_type = 'decision' AND subject_id = ?)
                 OR (subject_type = 'constraint' AND subject_id IN
                     (SELECT id FROM constraints WHERE decision_id = ?))
               )"#,
        )
        .bind(valid_to)
        .bind(decision_id)
        .bind(decision_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Soft-delete all active decisions and their constraints derived from an episode.
    /// Returns `(decisions_closed, constraints_closed)`.
    pub async fn retract_episode_facts(
        &self,
        episode_id: Uuid,
        valid_to: &str,
    ) -> Result<(i64, i64), Error> {
        let episode_id_str = episode_id.to_string();

        let d = sqlx::query(
            "UPDATE decisions SET valid_to = ? WHERE episode_id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(&episode_id_str)
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        // Cascade to constraints whose decision was just closed.
        let c = sqlx::query(
            r#"UPDATE constraints SET valid_to = ?
               WHERE decision_id IN (
                   SELECT id FROM decisions WHERE episode_id = ?
               )
               AND valid_to IS NULL"#,
        )
        .bind(valid_to)
        .bind(&episode_id_str)
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        // Close claims for every decision (and its constraints) from this episode.
        sqlx::query(
            r#"UPDATE claims SET valid_to = ?
               WHERE valid_to IS NULL AND (
                 (subject_type = 'decision' AND subject_id IN
                    (SELECT id FROM decisions WHERE episode_id = ?))
                 OR (subject_type = 'constraint' AND subject_id IN
                    (SELECT id FROM constraints WHERE decision_id IN
                       (SELECT id FROM decisions WHERE episode_id = ?)))
               )"#,
        )
        .bind(valid_to)
        .bind(&episode_id_str)
        .bind(&episode_id_str)
        .execute(&self.pool)
        .await?;

        Ok((d, c))
    }

    /// Insert a retraction-correction episode as an audit note.
    pub async fn insert_retraction_episode(
        &self,
        id: Uuid,
        content: &str,
        retracted_id: &str,
        entity_type: &str,
        now: &str,
    ) -> Result<(), Error> {
        let source = format!("retraction:{entity_type}:{retracted_id}");
        sqlx::query(
            r#"INSERT INTO episodes
               (id, source, source_uri, content, occurred_at, ingested_at, confidence, evidence_refs)
               VALUES (?, ?, NULL, ?, ?, ?, 1.0, '[]')"#,
        )
        .bind(id.to_string())
        .bind(&source)
        .bind(content)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // propose_links helpers
    // -----------------------------------------------------------------------

    /// Look up an ADR document by its canonical ADR ID string (e.g. "ADR-0042").
    pub async fn find_adr_document_by_adr_id(
        &self,
        repo_id: Uuid,
        adr_id: &str,
    ) -> Result<Option<crate::domain::entities::AdrDocument>, Error> {
        let rows = sqlx::query(
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
        .fetch_all(&self.pool)
        .await?;

        rows.first().map(|r| row_to_adr(r)).transpose()
    }

    /// Return the primary decision linked to an adr_document row, along with its raw embedding blob.
    pub async fn find_decision_for_adr(
        &self,
        repo_id: Uuid,
        adr_doc_id: Uuid,
    ) -> Result<Option<(String, Option<Vec<u8>>)>, Error> {
        let _ = repo_id; // repo is implicit via adr_documents.repo_id
        let row = sqlx::query(
            r#"SELECT id, embedding FROM decisions
               WHERE adr_id = ? AND valid_to IS NULL
               ORDER BY confidence DESC
               LIMIT 1"#,
        )
        .bind(adr_doc_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let id: String = r.get("id");
            let emb: Option<Vec<u8>> = r.get("embedding");
            (id, emb)
        }))
    }

    /// Return a single decision (with its embedding blob) by UUID.
    pub async fn find_decision_with_embedding(
        &self,
        repo_id: Uuid,
        decision_id: Uuid,
    ) -> Result<Option<(crate::domain::entities::DecisionSummary, Option<Vec<u8>>)>, Error> {
        let repo_id_str = repo_id.to_string();
        let row = sqlx::query(
            r#"SELECT d.id, d.text, d.valid_from, d.valid_to, d.confidence, d.embedding,
                      COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      d.episode_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      COALESCE(a.status, 'episode') AS status
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE d.id = ?
                 AND (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_to IS NULL
               LIMIT 1"#,
        )
        .bind(decision_id.to_string())
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let summary = crate::domain::entities::DecisionSummary {
                id: r.get("id"),
                adr_id: r.get("adr_id"),
                episode_id: r.get("episode_id"),
                title: r.get("title"),
                status: r.get("status"),
                text: r.get("text"),
                valid_from: r.get("valid_from"),
                valid_to: r.get("valid_to"),
                confidence: r.get("confidence"),
            };
            let emb: Option<Vec<u8>> = r.get("embedding");
            (summary, emb)
        }))
    }

    /// Return file paths explicitly mentioned by the code links of a decision.
    pub async fn file_mentions_for_decision(
        &self,
        repo_id: Uuid,
        decision_id: &str,
    ) -> Result<Vec<String>, Error> {
        let rows = sqlx::query(
            r#"SELECT DISTINCT dcl.file_path
               FROM decision_code_links dcl
               WHERE dcl.decision_id = ? AND dcl.repo_id = ?"#,
        )
        .bind(decision_id)
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(|r| r.get::<String, _>("file_path")).collect())
    }

    /// Fetch all commit rows with their text (for keyword matching).
    pub async fn fetch_commits_with_text(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String)>, Error> {
        // Returns (id, sha, message)
        let rows = sqlx::query(
            "SELECT id, sha, COALESCE(message, '') AS message FROM commits WHERE repo_id = ?",
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("sha"), r.get("message")))
            .collect())
    }

    /// Fetch all commit rows that have embeddings.
    pub async fn fetch_commits_with_embeddings_all(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String, Option<Vec<u8>>)>, Error> {
        // Returns (id, sha, message, embedding)
        let rows = sqlx::query(
            r#"SELECT id, sha, COALESCE(message, '') AS message, embedding
               FROM commits
               WHERE repo_id = ? AND embedding IS NOT NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("sha"), r.get("message"), r.get("embedding")))
            .collect())
    }

    /// Fetch symbols that live in any of the given repo-relative file paths.
    /// Returns (symbol_id, name, kind, file_path).
    pub async fn fetch_symbols_for_propose(
        &self,
        repo_id: Uuid,
        file_paths: &[String],
    ) -> Result<Vec<(String, String, String, String)>, Error> {
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        // SQLite doesn't support binding arrays directly; use IN clause with individual binds.
        let placeholders = file_paths.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT s.id, s.name, s.kind, f.path AS file_path
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ?
                 AND f.path IN ({})
                 AND s.valid_to IS NULL
               ORDER BY s.name"#,
            placeholders
        );
        let mut q = sqlx::query(&sql).bind(repo_id.to_string());
        for p in file_paths {
            q = q.bind(p);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("name"), r.get("kind"), r.get("file_path")))
            .collect())
    }

    /// Fetch routes that live in any of the given repo-relative file paths.
    /// Returns (route_id, method, path, file_path).
    pub async fn fetch_routes_for_propose(
        &self,
        repo_id: Uuid,
        file_paths: &[String],
    ) -> Result<Vec<(String, Option<String>, String, String)>, Error> {
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = file_paths.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT id, method, path, file_path
               FROM routes
               WHERE repo_id = ?
                 AND file_path IN ({})
                 AND valid_to IS NULL
               ORDER BY path"#,
            placeholders
        );
        let mut q = sqlx::query(&sql).bind(repo_id.to_string());
        for p in file_paths {
            q = q.bind(p);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("method"), r.get("path"), r.get("file_path")))
            .collect())
    }

    // -----------------------------------------------------------------------
    // find_stale_decisions helpers
    // -----------------------------------------------------------------------

    /// Decisions linked to file paths that are no longer present in the live `files` index.
    /// Returns (decision_id, adr_id, title, valid_from, file_path).
    pub async fn stale_decisions_deleted_files(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT DISTINCT d.id, COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      d.valid_from, dcl.file_path
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_to IS NULL
                 AND dcl.valid_to IS NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM files f
                     WHERE f.repo_id = ? AND f.path = dcl.file_path AND f.valid_to IS NULL
                 )
                 AND EXISTS (
                     SELECT 1 FROM files f
                     WHERE f.repo_id = ? AND f.path = dcl.file_path
                 )"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("adr_id"),
                    r.get("title"),
                    r.get("valid_from"),
                    r.get("file_path"),
                )
            })
            .collect())
    }

    /// Decisions linked to files that have been ingested but have no live symbols.
    /// Returns (decision_id, adr_id, title, valid_from, file_path).
    pub async fn stale_decisions_missing_symbols(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT DISTINCT d.id, COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      d.valid_from, dcl.file_path
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               JOIN files f ON f.repo_id = ? AND f.path = dcl.file_path AND f.valid_to IS NULL
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_to IS NULL
                 AND dcl.valid_to IS NULL
                 AND EXISTS (
                     SELECT 1 FROM symbols s WHERE s.file_id = f.id AND s.valid_to IS NOT NULL
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM symbols s WHERE s.file_id = f.id AND s.valid_to IS NULL
                 )"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("adr_id"),
                    r.get("title"),
                    r.get("valid_from"),
                    r.get("file_path"),
                )
            })
            .collect())
    }

    /// Active decisions with no linked commits after `since`.
    /// Returns (decision_id, adr_id, title, valid_from).
    pub async fn stale_decisions_no_recent_activity(
        &self,
        repo_id: Uuid,
        since: &str,
    ) -> Result<Vec<(String, String, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT d.id, COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      d.valid_from
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_to IS NULL
                 AND EXISTS (
                     SELECT 1 FROM decision_git_links dgl WHERE dgl.decision_id = d.id AND dgl.ref_type = 'commit'
                 )
                 AND NOT EXISTS (
                     SELECT 1
                     FROM decision_git_links dgl
                     JOIN commits c ON c.id = dgl.ref_id
                     WHERE dgl.decision_id = d.id AND dgl.ref_type = 'commit' AND c.source_time >= ?
                 )"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(since)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("adr_id"), r.get("title"), r.get("valid_from")))
            .collect())
    }

    /// Active accepted decisions whose ADR is older than `since` and not superseded.
    /// Returns (decision_id, adr_id, title, valid_from).
    pub async fn stale_decisions_aged(
        &self,
        repo_id: Uuid,
        since: &str,
    ) -> Result<Vec<(String, String, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT d.id, a.adr_id,
                      COALESCE(d.title, a.title) AS title,
                      d.valid_from
               FROM decisions d
               JOIN adr_documents a ON a.id = d.adr_id
               WHERE a.repo_id = ?
                 AND d.valid_to IS NULL
                 AND a.valid_to IS NULL
                 AND a.status = 'accepted'
                 AND a.superseded_by IS NULL
                 AND d.valid_from < ?"#,
        )
        .bind(repo_id.to_string())
        .bind(since)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("adr_id"), r.get("title"), r.get("valid_from")))
            .collect())
    }

    // -----------------------------------------------------------------------
    // trace_symbol_history helpers
    // -----------------------------------------------------------------------

    /// Return all ingested symbol spans whose name matches `symbol_name`.
    pub async fn trace_symbol_spans(
        &self,
        repo_id: Uuid,
        symbol_name: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<crate::storage::sqlite::SymbolSpan>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file_path,
                      s.valid_from, s.valid_to
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ?
                 AND s.name = ?
                 AND s.valid_from <= ?
                 AND (s.valid_to IS NULL OR s.valid_to >= ?)
               ORDER BY s.valid_from"#,
        )
        .bind(repo_id.to_string())
        .bind(symbol_name)
        .bind(to)
        .bind(from)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| SymbolSpan {
                id: r.get("id"),
                name: r.get("name"),
                kind: r.get("kind"),
                file_path: r.get("file_path"),
                valid_from: r.get("valid_from"),
                valid_to: r.get("valid_to"),
            })
            .collect())
    }

    /// Return decisions linked (via decision_code_links) to any of the given file paths.
    /// Returns (decision_id, adr_id, title, valid_from, valid_to, confidence).
    pub async fn trace_decisions_for_files(
        &self,
        repo_id: Uuid,
        file_paths: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, String, String, Option<String>, f64)>, Error> {
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = file_paths.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT DISTINCT d.id, COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      d.valid_from, d.valid_to, d.confidence
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND dcl.file_path IN ({})
                 AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to >= ?)
               ORDER BY d.valid_from"#,
            placeholders
        );
        let mut q = sqlx::query(&sql)
            .bind(repo_id.to_string())
            .bind(repo_id.to_string());
        for p in file_paths {
            q = q.bind(p);
        }
        q = q.bind(to).bind(from);
        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("adr_id"),
                    r.get("title"),
                    r.get("valid_from"),
                    r.get("valid_to"),
                    r.get("confidence"),
                )
            })
            .collect())
    }

    /// Return constraints belonging to any of the given decision IDs within the time window.
    /// Returns (constraint_id, text, valid_from, valid_to, confidence).
    pub async fn trace_constraints_for_decisions(
        &self,
        decision_ids: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, String, Option<String>, f64)>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = decision_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT id, text, valid_from, valid_to, confidence
               FROM constraints
               WHERE decision_id IN ({})
                 AND valid_from <= ?
                 AND (valid_to IS NULL OR valid_to >= ?)
               ORDER BY valid_from"#,
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id);
        }
        q = q.bind(to).bind(from);
        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("text"),
                    r.get("valid_from"),
                    r.get("valid_to"),
                    r.get("confidence"),
                )
            })
            .collect())
    }

    /// Return commits linked (via decision_git_links) to any of the given decision IDs.
    /// Returns (commit_id, sha, message, source_time, confidence).
    pub async fn trace_commits_for_decisions(
        &self,
        decision_ids: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, Option<String>, String, f64)>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = decision_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT DISTINCT c.id, c.sha, c.message, c.source_time,
                      dgl.confidence
               FROM decision_git_links dgl
               JOIN commits c ON c.id = dgl.ref_id
               WHERE dgl.decision_id IN ({})
                 AND dgl.ref_type = 'commit'
                 AND c.source_time >= ?
                 AND c.source_time <= ?
               ORDER BY c.source_time"#,
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id);
        }
        q = q.bind(from).bind(to);
        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("sha"),
                    r.get("message"),
                    r.get("source_time"),
                    r.get("confidence"),
                )
            })
            .collect())
    }

    /// Return episodes linked (via decisions.episode_id) to any of the given decision IDs.
    /// Returns (episode_id, source, content, occurred_at).
    pub async fn trace_episodes_for_decisions(
        &self,
        decision_ids: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, String, String)>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = decision_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            r#"SELECT DISTINCT e.id, e.source, e.content, e.occurred_at
               FROM decisions d
               JOIN episodes e ON e.id = d.episode_id
               WHERE d.id IN ({})
                 AND e.occurred_at >= ?
                 AND e.occurred_at <= ?
               ORDER BY e.occurred_at"#,
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id);
        }
        q = q.bind(from).bind(to);
        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("source"), r.get("content"), r.get("occurred_at")))
            .collect())
    }

    // -----------------------------------------------------------------------
    // Freshness helpers
    // -----------------------------------------------------------------------

    /// Returns the most recent `ingested_at` timestamp across all files for the
    /// given repo, or None if no files have been indexed yet.
    pub async fn last_file_ingested_at(&self, repo_id: Uuid) -> Result<Option<String>, Error> {
        let row = sqlx::query(
            "SELECT MAX(ingested_at) AS ts FROM files WHERE repo_id = ?",
        )
        .bind(repo_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("ts").unwrap_or(None))
    }

    // -----------------------------------------------------------------------
    // focused_file_brief helpers
    // -----------------------------------------------------------------------

    /// Fetch symbols in a file with line numbers for the brief output.
    pub async fn fetch_file_symbols_brief(
        &self,
        repo_id: Uuid,
        file_path: &str,
        valid_at: &str,
    ) -> Result<Vec<(String, String, Option<i64>)>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.name, s.kind, s.line
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ?
                 AND f.path = ?
                 AND s.valid_from <= ?
                 AND (s.valid_to IS NULL OR s.valid_to > ?)
               ORDER BY s.line ASC"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .bind(valid_at)
        .bind(valid_at)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("name"), r.get("kind"), r.get("line"))).collect())
    }

    /// Fetch cross-file callers: symbols in other files that call INTO this file.
    /// Returns (from_file, from_symbol_name) pairs ordered by confidence.
    pub async fn fetch_file_callers_brief(
        &self,
        repo_id: Uuid,
        file_path: &str,
        valid_at: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT DISTINCT s_from.name AS from_name, f_from.path AS from_file
               FROM symbol_edges se
               JOIN symbols s_to   ON s_to.id   = se.to_id
               JOIN files   f_to   ON f_to.id   = s_to.file_id
               JOIN symbols s_from ON s_from.id = se.from_id
               JOIN files   f_from ON f_from.id = s_from.file_id
               WHERE f_to.repo_id = ?
                 AND f_to.path    = ?
                 AND f_from.path != f_to.path
                 AND se.valid_from    <= ? AND (se.valid_to    IS NULL OR se.valid_to    > ?)
                 AND s_to.valid_from  <= ? AND (s_to.valid_to  IS NULL OR s_to.valid_to  > ?)
                 AND s_from.valid_from <= ? AND (s_from.valid_to IS NULL OR s_from.valid_to > ?)
               ORDER BY se.confidence DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .bind(valid_at).bind(valid_at)
        .bind(valid_at).bind(valid_at)
        .bind(valid_at).bind(valid_at)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("from_file"), r.get("from_name"))).collect())
    }

    /// Fetch cross-file callees: symbols in other files called FROM this file.
    /// Returns (to_file, to_symbol_name) pairs ordered by confidence.
    pub async fn fetch_file_callees_brief(
        &self,
        repo_id: Uuid,
        file_path: &str,
        valid_at: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT DISTINCT s_to.name AS to_name, f_to.path AS to_file
               FROM symbol_edges se
               JOIN symbols s_from ON s_from.id = se.from_id
               JOIN files   f_from ON f_from.id = s_from.file_id
               JOIN symbols s_to   ON s_to.id   = se.to_id
               JOIN files   f_to   ON f_to.id   = s_to.file_id
               WHERE f_from.repo_id = ?
                 AND f_from.path    = ?
                 AND f_to.path     != f_from.path
                 AND se.valid_from    <= ? AND (se.valid_to    IS NULL OR se.valid_to    > ?)
                 AND s_from.valid_from <= ? AND (s_from.valid_to IS NULL OR s_from.valid_to > ?)
                 AND s_to.valid_from  <= ? AND (s_to.valid_to  IS NULL OR s_to.valid_to  > ?)
               ORDER BY se.confidence DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .bind(valid_at).bind(valid_at)
        .bind(valid_at).bind(valid_at)
        .bind(valid_at).bind(valid_at)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("to_file"), r.get("to_name"))).collect())
    }
}
