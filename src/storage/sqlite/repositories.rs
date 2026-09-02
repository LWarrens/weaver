//! Repository rows and per-repo counts.
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
    // Repositories
    // -----------------------------------------------------------------------

    pub async fn upsert_repository(
        &self,
        path: &str,
        name: Option<&str>,
    ) -> Result<Repository, Error> {
        use chrono::Utc;
        let now = Utc::now().to_rfc3339();
        // Strip the Windows extended-length path prefix so all callers agree on the key.
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);

        let row =
            sqlx::query("SELECT id, path, name, ingested_at FROM repositories WHERE path = ?")
                .bind(path)
                .fetch_optional(&self.pool)
                .await?;

        if let Some(row) = row {
            return Ok(Repository {
                id: Uuid::parse_str(row.get("id")).map_err(|e| Error::Parse(e.to_string()))?,
                path: row.get("path"),
                name: row.get("name"),
                ingested_at: row.get("ingested_at"),
            });
        }

        let id = Uuid::new_v4();
        let id_str = id.to_string();
        sqlx::query("INSERT INTO repositories (id, path, name, ingested_at) VALUES (?, ?, ?, ?)")
            .bind(&id_str)
            .bind(path)
            .bind(name)
            .bind(&now)
            .execute(&self.pool)
            .await?;

        Ok(Repository {
            id,
            path: path.to_string(),
            name: name.map(str::to_string),
            ingested_at: now,
        })
    }

    pub async fn fetch_all_repositories(&self) -> Result<Vec<Repository>, Error> {
        let rows = sqlx::query(
            "SELECT id, path, name, ingested_at FROM repositories ORDER BY ingested_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(Repository {
                    id: Uuid::parse_str(row.get("id"))
                        .map_err(|e| Error::Parse(e.to_string()))?,
                    path: row.get("path"),
                    name: row.get("name"),
                    ingested_at: row.get("ingested_at"),
                })
            })
            .collect()
    }

    /// Delete a repository and all associated data.  Returns `true` if a row was
    /// actually removed (i.e. the path was known), `false` if not found.
    pub async fn delete_repository(&self, path: &str) -> Result<bool, Error> {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);

        let row = sqlx::query("SELECT id FROM repositories WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await?;

        let repo_id: String = match row {
            Some(r) => r.get("id"),
            None => return Ok(false),
        };

        let mut tx = self.pool.begin().await?;

        // ----- leaf tables (no dependents) first -----

        // commit_files → commits
        sqlx::query(
            "DELETE FROM commit_files WHERE commit_id IN \
             (SELECT id FROM commits WHERE repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // community_members → communities / symbols
        sqlx::query(
            "DELETE FROM community_members WHERE community_id IN \
             (SELECT id FROM communities WHERE repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // symbol_edges has its own repo_id column
        sqlx::query("DELETE FROM symbol_edges WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        // constraints → decisions → adr_documents
        sqlx::query(
            "DELETE FROM constraints WHERE decision_id IN \
             (SELECT d.id FROM decisions d \
              JOIN adr_documents a ON a.id = d.adr_id \
              WHERE a.repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // decision_code_links
        sqlx::query(
            "DELETE FROM decision_code_links WHERE decision_id IN \
             (SELECT d.id FROM decisions d \
              JOIN adr_documents a ON a.id = d.adr_id \
              WHERE a.repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // decision_git_links
        sqlx::query(
            "DELETE FROM decision_git_links WHERE decision_id IN \
             (SELECT d.id FROM decisions d \
              JOIN adr_documents a ON a.id = d.adr_id \
              WHERE a.repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // decisions
        sqlx::query(
            "DELETE FROM decisions WHERE adr_id IN \
             (SELECT id FROM adr_documents WHERE repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // supersession_edges
        sqlx::query(
            "DELETE FROM supersession_edges WHERE superseder_id IN \
             (SELECT id FROM adr_documents WHERE repo_id = ?) \
             OR superseded_id IN \
             (SELECT id FROM adr_documents WHERE repo_id = ?)",
        )
        .bind(&repo_id)
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // symbols → files
        sqlx::query(
            "DELETE FROM symbols WHERE file_id IN \
             (SELECT id FROM files WHERE repo_id = ?)",
        )
        .bind(&repo_id)
        .execute(&mut *tx)
        .await?;

        // routes has repo_id
        sqlx::query("DELETE FROM routes WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        // ----- tables with direct repo_id -----
        sqlx::query("DELETE FROM adr_documents WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM communities WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM entity_nodes WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM files WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM commits WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM pull_requests WHERE repo_id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        // ----- root -----
        sqlx::query("DELETE FROM repositories WHERE id = ?")
            .bind(&repo_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(true)
    }

    pub async fn table_count(&self, table: &str) -> Result<i64, Error> {
        if !is_known_table(table) {
            return Err(Error::InvalidInput {
                field: "table",
                reason: format!("unknown table: {}", table),
            });
        }

        let row = sqlx::query(&format!("SELECT COUNT(*) AS count FROM {}", table))
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("count"))
    }

    pub async fn table_columns(&self, table: &str) -> Result<Vec<SchemaColumn>, Error> {
        if !is_known_table(table) {
            return Err(Error::InvalidInput {
                field: "table",
                reason: format!("unknown table: {}", table),
            });
        }

        let rows = sqlx::query(&format!("PRAGMA table_info({})", table))
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .iter()
            .map(|row| SchemaColumn {
                name: row.get("name"),
                column_type: row.get("type"),
                not_null: row.get::<i64, _>("notnull") != 0,
                primary_key: row.get::<i64, _>("pk") != 0,
            })
            .collect())
    }

    pub async fn repository_counts(
        &self,
        repo_id: Uuid,
    ) -> Result<std::collections::BTreeMap<String, i64>, Error> {
        let mut counts = std::collections::BTreeMap::new();
        counts.insert(
            "adr_documents".to_string(),
            count_query(
                &self.pool,
                "SELECT COUNT(*) AS count FROM adr_documents WHERE repo_id = ?",
                repo_id,
                false,
            )
            .await?,
        );
        counts.insert(
            "decisions".to_string(),
            count_query(
                &self.pool,
                r#"SELECT COUNT(*) AS count
                   FROM decisions d
                   LEFT JOIN adr_documents a ON a.id = d.adr_id
                   LEFT JOIN episodes e ON e.id = d.episode_id
                   WHERE a.repo_id = ? OR e.repo_id = ?"#,
                repo_id,
                true,
            )
            .await?,
        );
        counts.insert(
            "constraints".to_string(),
            count_query(
                &self.pool,
                r#"SELECT COUNT(*) AS count
                   FROM constraints c
                   JOIN decisions d ON d.id = c.decision_id
                   LEFT JOIN adr_documents a ON a.id = d.adr_id
                   LEFT JOIN episodes e ON e.id = d.episode_id
                   WHERE a.repo_id = ? OR e.repo_id = ?"#,
                repo_id,
                true,
            )
            .await?,
        );
        counts.insert(
            "files".to_string(),
            count_query(
                &self.pool,
                "SELECT COUNT(*) AS count FROM files WHERE repo_id = ?",
                repo_id,
                false,
            )
            .await?,
        );
        counts.insert(
            "symbols".to_string(),
            count_query(
                &self.pool,
                r#"SELECT COUNT(*) AS count
                   FROM symbols s
                   JOIN files f ON f.id = s.file_id
                   WHERE f.repo_id = ?"#,
                repo_id,
                false,
            )
            .await?,
        );
        counts.insert(
            "episodes".to_string(),
            count_query(
                &self.pool,
                "SELECT COUNT(*) AS count FROM episodes WHERE repo_id = ?",
                repo_id,
                false,
            )
            .await?,
        );
        counts.insert(
            "decision_code_links".to_string(),
            count_query(
                &self.pool,
                r#"SELECT COUNT(*) AS count
                   FROM decision_code_links dcl
                   JOIN decisions d ON d.id = dcl.decision_id
                   LEFT JOIN adr_documents a ON a.id = d.adr_id
                   LEFT JOIN episodes e ON e.id = d.episode_id
                   WHERE a.repo_id = ? OR e.repo_id = ?"#,
                repo_id,
                true,
            )
            .await?,
        );
        counts.insert(
            "supersession_edges".to_string(),
            count_query(
                &self.pool,
                r#"SELECT COUNT(*) AS count
                   FROM supersession_edges se
                   JOIN adr_documents a ON a.id = se.superseder_id
                   WHERE a.repo_id = ?"#,
                repo_id,
                false,
            )
            .await?,
        );
        counts.insert(
            "temporal_edges".to_string(),
            self.table_count("temporal_edges").await?,
        );
        Ok(counts)
    }

}
