//! Files, symbols, symbol edges (call graph), and HTTP routes.
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
    // Files & Symbols (Phase 2)
    // -----------------------------------------------------------------------

    pub async fn upsert_file(
        &self,
        repo_id: Uuid,
        path: &str,
        ingested_at: &str,
        valid_from: &str,
    ) -> Result<Uuid, Error> {
        let row =
            sqlx::query("SELECT id FROM files WHERE repo_id = ? AND path = ? AND valid_to IS NULL")
                .bind(repo_id.to_string())
                .bind(path)
                .fetch_optional(&self.pool)
                .await?;

        if let Some(r) = row {
            let id_str: String = r.get("id");
            return Ok(Uuid::parse_str(&id_str).map_err(|e| Error::Parse(e.to_string()))?);
        }

        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO files (id, repo_id, path, ingested_at, valid_from) VALUES (?,?,?,?,?)",
        )
        .bind(id.to_string())
        .bind(repo_id.to_string())
        .bind(path)
        .bind(ingested_at)
        .bind(valid_from)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Return the stored `content_hash` for a live file record, or `None` if not yet set.
    pub async fn get_file_content_hash(
        &self,
        repo_id: Uuid,
        path: &str,
    ) -> Result<Option<String>, Error> {
        let row = sqlx::query(
            "SELECT content_hash FROM files WHERE repo_id = ? AND path = ? AND valid_to IS NULL LIMIT 1",
        )
        .bind(repo_id.to_string())
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("content_hash")))
    }

    /// Store (or update) the `content_hash` for a file that has just been processed.
    pub async fn update_file_content_hash(
        &self,
        file_id: Uuid,
        hash: &str,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE files SET content_hash = ? WHERE id = ?")
            .bind(hash)
            .bind(file_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn find_file_id_by_path(&self, repo_id: Uuid, path: &str) -> Result<Option<Uuid>, Error> {
        let row = sqlx::query("SELECT id FROM files WHERE repo_id = ? AND path = ? AND valid_to IS NULL")
            .bind(repo_id.to_string())
            .bind(path)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(r) = row {
            let id_str: String = r.get("id");
            Ok(Some(Uuid::parse_str(&id_str).map_err(|e| Error::Parse(e.to_string()))?))
        } else {
            Ok(None)
        }
    }

    pub async fn insert_symbol(
        &self,
        file_id: Uuid,
        name: &str,
        kind: &str,
        line: i64,
        end_line: i64,
        ingested_at: &str,
        valid_from: &str,
        signature: Option<&str>,
        return_type: Option<&str>,
        visibility: Option<&str>,
        is_async: bool,
        complexity: Option<i64>,
        decorators: Option<&str>,
    ) -> Result<(), Error> {
        let current = sqlx::query(
            r#"SELECT id, line, end_line FROM symbols
               WHERE file_id = ? AND name = ? AND kind = ? AND valid_to IS NULL
               LIMIT 1"#,
        )
        .bind(file_id.to_string())
        .bind(name)
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = current {
            let id: String = row.get("id");
            let current_line: Option<i64> = row.get("line");
            let current_end_line: Option<i64> = row.get("end_line");
            if current_line != Some(line) || current_end_line != Some(end_line) {
                sqlx::query(
                    "UPDATE symbols SET line = ?, end_line = ?, ingested_at = ?,
                     signature = ?, return_type = ?, visibility = ?,
                     is_async = ?, complexity = ?, decorators = ?
                     WHERE id = ?",
                )
                .bind(line)
                .bind(end_line)
                .bind(ingested_at)
                .bind(signature)
                .bind(return_type)
                .bind(visibility)
                .bind(is_async as i64)
                .bind(complexity)
                .bind(decorators)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            return Ok(());
        }

        let id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT OR IGNORE INTO symbols
               (id, file_id, name, kind, line, end_line, ingested_at, valid_from,
                signature, return_type, visibility, is_async, complexity, decorators)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(&id)
        .bind(file_id.to_string())
        .bind(name)
        .bind(kind)
        .bind(line)
        .bind(end_line)
        .bind(ingested_at)
        .bind(valid_from)
        .bind(signature)
        .bind(return_type)
        .bind(visibility)
        .bind(is_async as i64)
        .bind(complexity)
        .bind(decorators)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn close_stale_symbols_for_file(
        &self,
        file_id: Uuid,
        current_symbols: &[(String, String)],
        valid_to: &str,
    ) -> Result<(), Error> {
        let rows = sqlx::query(
            "SELECT id, name, kind FROM symbols WHERE file_id = ? AND valid_to IS NULL",
        )
        .bind(file_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        for row in rows {
            let name: String = row.get("name");
            let kind: String = row.get("kind");
            let is_current = current_symbols
                .iter()
                .any(|(current_name, current_kind)| current_name == &name && current_kind == &kind);

            if !is_current {
                let id: String = row.get("id");
                sqlx::query("UPDATE symbols SET valid_to = ? WHERE id = ?")
                    .bind(valid_to)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        Ok(())
    }

    /// Set `valid_to` on a specific open symbol. No-op if no open record matches.
    pub async fn close_symbol_by_name(
        &self,
        file_id: Uuid,
        name: &str,
        kind: &str,
        valid_to: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE symbols SET valid_to = ? WHERE file_id = ? AND name = ? AND kind = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(file_id.to_string())
        .bind(name)
        .bind(kind)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Back-date `valid_from` on an open symbol if the stored value is later than `new_valid_from`.
    pub async fn backdate_symbol_valid_from(
        &self,
        file_id: Uuid,
        name: &str,
        kind: &str,
        new_valid_from: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE symbols SET valid_from = ? WHERE file_id = ? AND name = ? AND kind = ? AND valid_to IS NULL AND valid_from > ?",
        )
        .bind(new_valid_from)
        .bind(file_id.to_string())
        .bind(name)
        .bind(kind)
        .bind(new_valid_from)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_files_with_symbol(
        &self,
        repo_id: Uuid,
        symbol_name: &str,
        valid_at: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");

        let rows = sqlx::query(
            r#"SELECT f.path
               FROM files f
               JOIN symbols s ON s.file_id = f.id
               WHERE f.repo_id = ?
                 AND s.name = ?
                 AND s.valid_from <= ?
                 AND (s.valid_to IS NULL OR s.valid_to > ?)"#,
        )
        .bind(repo_id.to_string())
        .bind(symbol_name)
        .bind(at)
        .bind(at)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(|r| r.get::<String, _>("path")).collect())
    }

    /// Resolve a live symbol name to `(file_path, start_line, end_line)` when
    /// exactly one live symbol carries that name. Used by anchor verification to
    /// re-locate a `SymbolQn` anchor at the current index.
    pub async fn resolve_symbol_span(
        &self,
        repo_id: Uuid,
        name: &str,
    ) -> Result<Option<(String, u32, u32)>, Error> {
        let rows = sqlx::query(
            r#"SELECT f.path AS path, s.line AS line, s.end_line AS end_line
               FROM symbols s JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.name = ? AND s.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .bind(name)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() != 1 {
            return Ok(None);
        }
        let r = &rows[0];
        let path: String = r.get("path");
        let line: i64 = r.get("line");
        let end_line: Option<i64> = r.get("end_line");
        let start = line.max(1) as u32;
        let end = end_line.unwrap_or(line).max(line) as u32;
        Ok(Some((path, start, end)))
    }
}

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Symbol edges (call graph)
    // -----------------------------------------------------------------------

    /// Return the UUID of a live symbol in `file_id` matching `name` and `kind` prefix.
    pub async fn find_symbol_id_in_file(
        &self,
        file_id: Uuid,
        name: &str,
    ) -> Result<Option<Uuid>, Error> {
        let row = sqlx::query(
            "SELECT id FROM symbols WHERE file_id = ? AND name = ? AND valid_to IS NULL LIMIT 1",
        )
        .bind(file_id.to_string())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| {
            let s: String = r.get("id");
            Uuid::parse_str(&s).map_err(|e| Error::Parse(e.to_string()))
        })
        .transpose()
    }

    /// Return the UUID of the unique symbol with `name` in `repo_id`, if exactly one exists.
    /// Confidence: 0.75 (repo-wide unique name, tier 3).
    pub async fn find_unique_symbol_id_in_repo(
        &self,
        repo_id: Uuid,
        name: &str,
    ) -> Result<Option<Uuid>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.id FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.name = ? AND s.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .bind(name)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() != 1 {
            return Ok(None);
        }

        let s: String = rows[0].get("id");
        Ok(Some(
            Uuid::parse_str(&s).map_err(|e| Error::Parse(e.to_string()))?,
        ))
    }

    /// Insert a symbol edge. If an identical live edge already exists, skip.
    pub async fn insert_symbol_edge(&self, edge: &SymbolEdge) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO symbol_edges
               (id, repo_id, from_id, to_id, to_name, edge_type, confidence, valid_from)
               VALUES (?,?,?,?,?,?,?,?)"#,
        )
        .bind(edge.id.to_string())
        .bind(edge.repo_id.to_string())
        .bind(edge.from_id.to_string())
        .bind(edge.to_id.map(|u| u.to_string()))
        .bind(&edge.to_name)
        .bind(&edge.edge_type)
        .bind(edge.confidence)
        .bind(&edge.valid_from)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove all live edges originating from `from_id` (when a file is re-ingested).
    pub async fn close_stale_edges_for_symbol(
        &self,
        from_id: Uuid,
        valid_to: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE symbol_edges SET valid_to = ? WHERE from_id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(from_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch all live symbol edges with unresolved targets (to_id IS NULL).
    /// Returns (edge_id, from_id, to_name, edge_type) tuples.
    pub async fn fetch_unresolved_symbol_edges(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, Uuid, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT id AS edge_id, from_id, to_name, edge_type
               FROM symbol_edges
               WHERE repo_id = ? AND to_id IS NULL AND valid_to IS NULL
                 AND to_name IS NOT NULL
               LIMIT 10000"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let edge_id_s: String = row.get("edge_id");
            let from_id_s: String = row.get("from_id");
            let to_name: String = row.get("to_name");
            let edge_type: String = row.get("edge_type");
            if let (Ok(eid), Ok(fid)) =
                (Uuid::parse_str(&edge_id_s), Uuid::parse_str(&from_id_s))
            {
                result.push((eid, fid, to_name, edge_type));
            }
        }
        Ok(result)
    }

    /// Fill in a previously-unresolved symbol edge with a resolved target.
    pub async fn resolve_symbol_edge(
        &self,
        edge_id: Uuid,
        to_id: Uuid,
        confidence: f64,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE symbol_edges SET to_id = ?, confidence = ? WHERE id = ?")
            .bind(to_id.to_string())
            .bind(confidence)
            .bind(edge_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Given a set of file paths, return the set of file paths that have symbols
    /// which call symbols defined in the given files (one hop). Used for expanding
    /// the affected-file set in `inspect_change`.
    pub async fn find_one_hop_caller_files(
        &self,
        repo_id: Uuid,
        file_paths: &[String],
    ) -> Result<Vec<String>, Error> {
        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build placeholders for the IN clause.
        let placeholders = file_paths
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");

        let sql = format!(
            r#"SELECT DISTINCT f_caller.path
               FROM files f_callee
               JOIN symbols s_callee ON s_callee.file_id = f_callee.id
               JOIN symbol_edges se ON se.to_id = s_callee.id
               JOIN symbols s_caller ON s_caller.id = se.from_id
               JOIN files f_caller ON f_caller.id = s_caller.file_id
               WHERE f_callee.repo_id = ?
                 AND f_callee.path IN ({})
                 AND s_callee.valid_to IS NULL
                 AND se.valid_to IS NULL
                 AND s_caller.valid_to IS NULL"#,
            placeholders
        );

        let mut q = sqlx::query(&sql).bind(repo_id.to_string());
        for path in file_paths {
            q = q.bind(path.as_str());
        }

        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("path")).collect())
    }

    /// Return true if the repository has any symbol edges (call graph available).
    pub async fn has_symbol_edges(&self, repo_id: Uuid) -> Result<bool, Error> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS count FROM symbol_edges WHERE repo_id = ? AND valid_to IS NULL LIMIT 1",
        )
        .bind(repo_id.to_string())
        .fetch_one(&self.pool)
        .await?;

        let count: i64 = row.get("count");
        Ok(count > 0)
    }

    // -----------------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------------

    pub async fn insert_route(
        &self,
        id: Uuid,
        repo_id: Uuid,
        method: Option<&str>,
        path: &str,
        framework: Option<&str>,
        handler_id: Option<Uuid>,
        file_path: &str,
        line: i64,
        confidence: f64,
        valid_from: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO routes
               (id, repo_id, method, path, framework, handler_id, file_path, line, confidence, valid_from)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(id.to_string())
        .bind(repo_id.to_string())
        .bind(method)
        .bind(path)
        .bind(framework)
        .bind(handler_id.map(|h| h.to_string()))
        .bind(file_path)
        .bind(line)
        .bind(confidence)
        .bind(valid_from)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn close_stale_routes_for_file(
        &self,
        repo_id: Uuid,
        file_path: &str,
        valid_to: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE routes SET valid_to = ? WHERE repo_id = ? AND file_path = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(repo_id.to_string())
        .bind(file_path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_routes_for_repo(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, Option<String>, String, Option<String>)>, Error> {
        let rows = sqlx::query(
            "SELECT file_path, method, path, framework FROM routes WHERE repo_id = ? AND valid_to IS NULL",
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let result = rows
            .into_iter()
            .map(|r| {
                let file_path: String = r.get("file_path");
                let method: Option<String> = r.get("method");
                let path: String = r.get("path");
                let framework: Option<String> = r.get("framework");
                (file_path, method, path, framework)
            })
            .collect();
        Ok(result)
    }

}
