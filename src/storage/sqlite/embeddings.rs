//! Embedding storage and backfill helpers.
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
    // Embedding helpers
    // -----------------------------------------------------------------------

    /// Persist a packed f32 embedding blob for a decision row.
    pub async fn update_decision_embedding(
        &self,
        decision_id: Uuid,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE decisions SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(decision_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a packed f32 embedding blob for a constraint row.
    pub async fn update_constraint_embedding(
        &self,
        constraint_id: Uuid,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE constraints SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(constraint_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a packed f32 embedding blob for a symbol row.
    pub async fn update_symbol_embedding(
        &self,
        symbol_id: Uuid,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE symbols SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(symbol_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a packed f32 embedding blob for an episode row.
    pub async fn update_episode_embedding(
        &self,
        episode_id: Uuid,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE episodes SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(episode_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a packed f32 embedding blob for a commit row.
    pub async fn update_commit_embedding(
        &self,
        commit_id: &str,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE commits SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(commit_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a packed f32 embedding blob for an entity_node row.
    pub async fn update_entity_node_embedding(
        &self,
        node_id: Uuid,
        embedding: &[u8],
    ) -> Result<(), Error> {
        sqlx::query("UPDATE entity_nodes SET embedding = ? WHERE id = ?")
            .bind(embedding)
            .bind(node_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Fetch all decisions missing an embedding for a repository. Used by embed_all.
    pub async fn fetch_decisions_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let repo_id_str = repo_id.to_string();
        let rows = sqlx::query(
            r#"SELECT d.id, d.text
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.embedding IS NULL
                 AND d.valid_to IS NULL"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?, r.get("text")))
            })
            .collect()
    }

    /// Fetch all constraints missing an embedding for a repository.
    pub async fn fetch_constraints_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let repo_id_str = repo_id.to_string();
        let rows = sqlx::query(
            r#"SELECT c.id, c.text
               FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND c.embedding IS NULL"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?, r.get("text")))
            })
            .collect()
    }

    /// Fetch all episodes missing an embedding for a repository.
    pub async fn fetch_episodes_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let rows = sqlx::query(
            "SELECT id, content FROM episodes WHERE repo_id = ? AND embedding IS NULL",
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?, r.get("content")))
            })
            .collect()
    }

    /// Fetch all commits missing an embedding for a repository.
    pub async fn fetch_commits_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let rows = sqlx::query(
            "SELECT id, message FROM commits WHERE repo_id = ? AND embedding IS NULL",
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?, r.get("message")))
            })
            .collect()
    }

    /// Fetch all symbols missing an embedding for a repository.
    pub async fn fetch_symbols_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.id, s.name
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.embedding IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?, r.get("name")))
            })
            .collect()
    }

    /// Fetch all entity nodes missing an embedding for a repository. Used by embed_all.
    pub async fn fetch_entity_nodes_without_embeddings(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT id, canonical_name
               FROM entity_nodes
               WHERE repo_id = ? AND embedding IS NULL AND valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let id: String = r.get("id");
                Ok((
                    Uuid::parse_str(&id)
                        .map_err(|_| anyhow::anyhow!("invalid uuid in storage"))?,
                    r.get("canonical_name"),
                ))
            })
            .collect()
    }

    /// Fetch all active decisions for a repository together with their embedding blobs.
    /// Used for cosine-similarity search.
    pub async fn fetch_decisions_with_embeddings(
        &self,
        repo_id: Uuid,
        valid_at: Option<&str>,
    ) -> Result<Vec<(DecisionSummary, Option<Vec<u8>>)>, Error> {
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");
        let repo_id_str = repo_id.to_string();

        let rows = sqlx::query(
            r#"SELECT d.id, d.text, d.valid_from, d.valid_to, d.confidence,
                      COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      d.episode_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      COALESCE(a.status, 'episode') AS status,
                      d.embedding
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to > ?)"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .bind(at)
        .bind(at)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                let summary = DecisionSummary {
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
                let blob: Option<Vec<u8>> = r.get("embedding");
                (summary, blob)
            })
            .collect())
    }

    /// Return all active decisions for a repository (used by inspect_change to check all constraints).
    pub async fn list_all_decisions(
        &self,
        repo_id: Uuid,
        valid_at: Option<&str>,
    ) -> Result<Vec<DecisionSummary>, Error> {
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");

        let rows = sqlx::query(
            r#"SELECT d.id, d.text, d.valid_from, d.valid_to, d.confidence,
                      COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      d.episode_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      COALESCE(a.status, 'episode') AS status
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to > ?)
               ORDER BY d.confidence DESC"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(at)
        .bind(at)
        .fetch_all(&self.pool)
        .await?;

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

    /// Return the next sequential ADR number for a repository (max existing + 1).
    pub async fn next_adr_number(&self, repo_id: Uuid) -> Result<u32, Error> {
        let row = sqlx::query(
            r#"SELECT MAX(CAST(SUBSTR(adr_id, INSTR(adr_id, '-') + 1) AS INTEGER)) AS max_n
               FROM adr_documents
               WHERE repo_id = ? AND valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let n: u32 = row
            .and_then(|r| r.try_get::<Option<i64>, _>("max_n").ok().flatten())
            .map(|n| n as u32)
            .unwrap_or(0);

        Ok(n + 1)
    }

}
