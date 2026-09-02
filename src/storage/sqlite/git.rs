//! Commit ingestion, commit files, and decision-git links.
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
    // Commit ingestion
    // -----------------------------------------------------------------------

    /// Insert a commit record. Returns true if inserted, false if already existed (idempotent).
    pub async fn insert_commit(
        &self,
        repo_id: Uuid,
        sha: &str,
        author: Option<&str>,
        message: Option<&str>,
        source_time: &str,
        ingested_at: &str,
    ) -> Result<bool, Error> {
        let id = Uuid::new_v4().to_string();
        let result = sqlx::query(
            r#"INSERT OR IGNORE INTO commits
               (id, repo_id, sha, author, message, source_time, ingested_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(repo_id.to_string())
        .bind(sha)
        .bind(author)
        .bind(message)
        .bind(source_time)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Fetch the UUID of an existing commit by repo + sha, if present.
    pub async fn find_commit_id_by_sha(
        &self,
        repo_id: Uuid,
        sha: &str,
    ) -> Result<Option<String>, Error> {
        let row = sqlx::query_scalar(
            "SELECT id FROM commits WHERE repo_id = ? AND sha = ? LIMIT 1",
        )
        .bind(repo_id.to_string())
        .bind(sha)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Insert a decision–commit link. Ignores duplicates.
    pub async fn insert_decision_git_link(
        &self,
        decision_id: &str,
        ref_id: &str,
        confidence: f64,
        valid_from: &str,
        ingested_at: &str,
        link_source: &str,
    ) -> Result<(), Error> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT OR IGNORE INTO decision_git_links
               (id, decision_id, ref_type, ref_id, link_source, confidence, valid_from, ingested_at)
               VALUES (?, ?, 'commit', ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(decision_id)
        .bind(ref_id)
        .bind(link_source)
        .bind(confidence)
        .bind(valid_from)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    /// Fetch symbol names and kinds for a given relative file path in a repo.
    pub async fn fetch_symbols_for_file(
        &self,
        repo_id: Uuid,
        file_path: &str,
    ) -> Result<Vec<(String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.name, s.kind
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND f.path = ? AND s.valid_to IS NULL
               ORDER BY s.line ASC"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("name"), r.get("kind"))).collect())
    }

    // -----------------------------------------------------------------------
    /// Insert a commit->file mapping. Ignores duplicates.
    pub async fn insert_commit_file(
        &self,
        commit_id: &str,
        file_path: &str,
        ingested_at: &str,
    ) -> Result<(), Error> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT OR IGNORE INTO commit_files (id, commit_id, file_path, ingested_at) VALUES (?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(commit_id)
        .bind(file_path)
        .bind(ingested_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch recent commits touching a file path for a given repo. Returns tuples of (commit_id, sha, author, message, source_time)
    pub async fn fetch_recent_commits_for_file(
        &self,
        repo_id: Uuid,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, Option<String>, Option<String>, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT c.id, c.sha, c.author, c.message, c.source_time
               FROM commits c
               JOIN commit_files cf ON cf.commit_id = c.id
               WHERE c.repo_id = ? AND cf.file_path = ?
               ORDER BY c.source_time DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id")?;
            let sha: String = r.try_get("sha")?;
            let author: Option<String> = r.try_get("author")?;
            let message: Option<String> = r.try_get("message")?;
            let source_time: String = r.try_get("source_time")?;
            out.push((id, sha, author, message, source_time));
        }
        Ok(out)
    }

    /// Fetch files that are frequently co-changed with a given file path, ordered by co-change count desc.
    pub async fn fetch_cochanged_files(
        &self,
        repo_id: Uuid,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<(String, u32)>, Error> {
        let rows = sqlx::query(
            r#"SELECT cf2.file_path AS peer, COUNT(*) AS co_count
               FROM commit_files cf1
               JOIN commit_files cf2 ON cf1.commit_id = cf2.commit_id
                                    AND cf1.file_path != cf2.file_path
               JOIN commits c ON c.id = cf1.commit_id
               WHERE c.repo_id = ? AND cf1.file_path = ?
               GROUP BY cf2.file_path
               HAVING COUNT(*) >= 2
               ORDER BY co_count DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(file_path)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("peer"), r.get::<i64, _>("co_count") as u32))
            .collect())
    }

    /// Fetch a relationship graph for UI inspection.
    pub async fn fetch_graph_snapshot(
        &self,
        repo_id: Uuid,
        valid_at: &str,
        max_nodes_per_kind: Option<usize>,
        max_edges: Option<usize>,
    ) -> Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>), Error> {
        let mut nodes: std::collections::HashMap<String, GraphNodeRow> =
            std::collections::HashMap::new();
        let mut edges: Vec<GraphEdgeRow> = Vec::new();
        let node_limit = sqlite_limit(max_nodes_per_kind);
        let double_node_limit =
            sqlite_limit(max_nodes_per_kind.map(|limit| limit.saturating_mul(2)));
        let edge_limit = sqlite_limit(max_edges);
        let cochange_limit = sqlite_limit(max_edges.map(|limit| limit / 4));

        let mut upsert_node = |node: GraphNodeRow| {
            nodes.entry(node.id.clone()).or_insert(node);
        };

        let decision_rows = sqlx::query(
            r#"SELECT d.id AS decision_id,
                      COALESCE(ad.adr_id, 'lead') AS adr_key,
                      COALESCE(d.title, ad.title, e.source) AS title,
                      d.text AS text,
                      CASE WHEN ad.id IS NOT NULL THEN 'decision' ELSE 'lead' END AS node_kind
               FROM decisions d
               LEFT JOIN adr_documents ad ON ad.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (ad.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to > ?)
               ORDER BY d.valid_from DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(node_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in &decision_rows {
            let id: String = row.get("decision_id");
            let adr_key: String = row.get("adr_key");
            let title: String = row.get("title");
            let text: String = row.get("text");
            let node_kind: String = row.get("node_kind");
            upsert_node(GraphNodeRow {
                id,
                kind: node_kind,
                label: format!("{} {}", adr_key, title),
                detail: Some(text.chars().take(180).collect()),
            });
        }

        let constraint_rows = sqlx::query(
            r#"SELECT c.id AS constraint_id, c.decision_id AS decision_id, c.text AS text, c.confidence AS confidence
               FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents ad ON ad.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (ad.repo_id = ? OR e.repo_id = ?)
                 AND c.valid_from <= ?
                 AND (c.valid_to IS NULL OR c.valid_to > ?)
               ORDER BY c.valid_from DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(node_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in constraint_rows {
            let id: String = row.get("constraint_id");
            let decision_id: String = row.get("decision_id");
            let text: String = row.get("text");
            let confidence: f64 = row.get("confidence");
            upsert_node(GraphNodeRow {
                id: id.clone(),
                kind: "constraint".to_string(),
                label: text.chars().take(80).collect(),
                detail: Some(text),
            });
            if within_budget(edges.len(), max_edges) {
                edges.push(GraphEdgeRow {
                    id: format!("constraint:{}:{}", id, decision_id),
                    source: id,
                    target: decision_id,
                    edge_type: "imposes".to_string(),
                    confidence,
                    cross_file: false,
                });
            }
        }

        let code_rows = sqlx::query(
            r#"SELECT dcl.id AS link_id, dcl.decision_id AS decision_id, dcl.file_path AS file_path,
                      dcl.link_type AS link_type, dcl.confidence AS confidence
               FROM decision_code_links dcl
               JOIN decisions d ON d.id = dcl.decision_id
               LEFT JOIN adr_documents ad ON ad.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (ad.repo_id = ? OR e.repo_id = ?)
                 AND dcl.valid_from <= ?
                 AND (dcl.valid_to IS NULL OR dcl.valid_to > ?)
               ORDER BY dcl.confidence DESC, dcl.valid_from DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(edge_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in code_rows {
            let id: String = row.get("link_id");
            let decision_id: String = row.get("decision_id");
            let file_path: String = row.get("file_path");
            let link_type: String = row.get("link_type");
            let confidence: f64 = row.get("confidence");
            let file_id = format!("file:{}", file_path);
            upsert_node(GraphNodeRow {
                id: file_id.clone(),
                kind: "file".to_string(),
                label: file_path.clone(),
                detail: Some(file_path),
            });
            if within_budget(edges.len(), max_edges) {
                edges.push(GraphEdgeRow {
                    id,
                    source: decision_id,
                    target: file_id,
                    edge_type: link_type,
                    confidence,
                    cross_file: false,
                });
            }
        }

        let git_rows = sqlx::query(
            r#"SELECT dgl.id AS link_id, dgl.decision_id AS decision_id, c.id AS commit_id, c.sha AS sha,
                      c.message AS message, dgl.confidence AS confidence
               FROM decision_git_links dgl
               JOIN decisions d ON d.id = dgl.decision_id
               LEFT JOIN adr_documents ad ON ad.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               JOIN commits c ON c.id = dgl.ref_id
               WHERE (ad.repo_id = ? OR e.repo_id = ?)
                 AND dgl.valid_from <= ?
                 AND (dgl.valid_to IS NULL OR dgl.valid_to > ?)
               ORDER BY dgl.confidence DESC, dgl.valid_from DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(edge_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in git_rows {
            let id: String = row.get("link_id");
            let decision_id: String = row.get("decision_id");
            let commit_id: String = row.get("commit_id");
            let sha: String = row.get("sha");
            let message: Option<String> = row.get("message");
            let confidence: f64 = row.get("confidence");
            upsert_node(GraphNodeRow {
                id: commit_id.clone(),
                kind: "commit".to_string(),
                label: sha.chars().take(8).collect(),
                detail: message,
            });
            if within_budget(edges.len(), max_edges) {
                edges.push(GraphEdgeRow {
                    id,
                    source: decision_id,
                    target: commit_id,
                    edge_type: "references_commit".to_string(),
                    confidence,
                    cross_file: false,
                });
            }
        }

        // Standalone commits: visible regardless of whether they have decision links
        let standalone_commit_rows = sqlx::query(
            r#"SELECT c.id AS commit_id, c.sha AS sha, c.message AS message,
                      cf.id AS cf_id, cf.file_path AS file_path
               FROM commits c
               JOIN commit_files cf ON cf.commit_id = c.id
               WHERE c.repo_id = ?
               ORDER BY c.source_time DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(double_node_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in standalone_commit_rows {
            let commit_id: String = row.get("commit_id");
            let sha: String = row.get("sha");
            let message: Option<String> = row.get("message");
            let cf_id: String = row.get("cf_id");
            let file_path: String = row.get("file_path");
            let file_id = format!("file:{}", file_path);
            upsert_node(GraphNodeRow {
                id: commit_id.clone(),
                kind: "commit".to_string(),
                label: sha.chars().take(8).collect(),
                detail: message,
            });
            upsert_node(GraphNodeRow {
                id: file_id.clone(),
                kind: "file".to_string(),
                label: file_path.clone(),
                detail: Some(file_path),
            });
            // structural edges: always include, don't count against call-edge budget
            edges.push(GraphEdgeRow {
                id: format!("modifies:{}", cf_id),
                source: commit_id,
                target: file_id,
                edge_type: "modifies".to_string(),
                confidence: 1.0,
                cross_file: false,
            });
        }

        // Cross-file call edges are prioritised: architecture-spanning calls are more
        // interesting than same-file calls, and they were previously squeezed out by
        // same-file edges at higher confidence filling the shared budget first.
        let symbol_edge_rows = sqlx::query(
            r#"SELECT se.id AS edge_id, se.from_id AS from_id, se.to_id AS to_id, se.edge_type AS edge_type,
                      se.confidence AS confidence,
                      from_sym.name AS from_name, from_sym.kind AS from_kind, from_file.path AS from_file,
                      to_sym.name AS to_name, to_sym.kind AS to_kind, to_file.path AS to_file
               FROM symbol_edges se
               JOIN symbols from_sym ON from_sym.id = se.from_id
               JOIN files from_file ON from_file.id = from_sym.file_id
               LEFT JOIN symbols to_sym ON to_sym.id = se.to_id
               LEFT JOIN files to_file ON to_file.id = to_sym.file_id
               WHERE se.repo_id = ?
                 AND se.valid_from <= ?
                 AND (se.valid_to IS NULL OR se.valid_to > ?)
                 AND se.to_id IS NOT NULL
               ORDER BY (CASE WHEN from_file.path != COALESCE(to_file.path, '') THEN 1 ELSE 0 END) DESC,
                        se.confidence DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(edge_limit)
        .fetch_all(&self.pool)
        .await?;

        // Use a separate counter so file-scoped edges (modifies, co-change) don't eat the
        // symbol-edge budget. Nodes are always upserted even when the edge budget is full.
        let mut symbol_edge_count = 0usize;
        for row in symbol_edge_rows {
            let id: String = row.get("edge_id");
            let from_id: String = row.get("from_id");
            let to_id: String = row.get("to_id");
            let edge_type: String = row.get("edge_type");
            let confidence: f64 = row.get("confidence");
            let from_name: String = row.get("from_name");
            let from_kind: String = row.get("from_kind");
            let from_file: String = row.get("from_file");
            let to_name: String = row.get("to_name");
            let to_kind: String = row.get("to_kind");
            let to_file: String = row.get("to_file");
            upsert_node(GraphNodeRow {
                id: from_id.clone(),
                kind: "symbol".to_string(),
                label: from_name,
                detail: Some(format!("{} · {}", from_kind, from_file)),
            });
            upsert_node(GraphNodeRow {
                id: to_id.clone(),
                kind: "symbol".to_string(),
                label: to_name,
                detail: Some(format!("{} · {}", to_kind, to_file)),
            });
            if within_budget(symbol_edge_count, max_edges) {
                edges.push(GraphEdgeRow {
                    id,
                    source: from_id,
                    target: to_id,
                    edge_type,
                    confidence,
                    cross_file: from_file != to_file,
                });
                symbol_edge_count += 1;
            }
        }

        // Standalone symbols: always include a representative sample so the graph
        // is populated even when no edges have been resolved yet (e.g. mid-ingest
        // or when the language's edge extractor produces no matches).
        // Symbols already inserted via the edge walk are deduped by upsert_node.
        let standalone_symbol_rows = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file_path
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ?
                 AND s.valid_from <= ?
                 AND (s.valid_to IS NULL OR s.valid_to > ?)
               ORDER BY s.ingested_at DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .bind(node_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in standalone_symbol_rows {
            let id: String = row.get("id");
            let name: String = row.get("name");
            let kind: String = row.get("kind");
            let file_path: String = row.get("file_path");
            upsert_node(GraphNodeRow {
                id,
                kind: "symbol".to_string(),
                label: name,
                detail: Some(format!("{} · {}", kind, file_path)),
            });
        }

        // Co-change edges: files frequently modified together in the same commit.
        // Strong coupling signal — "when you touch A, you usually touch B too."
        let cochange_rows = sqlx::query(
            r#"SELECT cf1.file_path AS file_a, cf2.file_path AS file_b, COUNT(*) AS co_count
               FROM commit_files cf1
               JOIN commit_files cf2 ON cf1.commit_id = cf2.commit_id
                                     AND cf1.file_path < cf2.file_path
               JOIN commits c ON c.id = cf1.commit_id
               WHERE c.repo_id = ?
               GROUP BY cf1.file_path, cf2.file_path
               HAVING COUNT(*) >= 2
               ORDER BY co_count DESC
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(cochange_limit)
        .fetch_all(&self.pool)
        .await?;

        for row in cochange_rows {
            let file_a: String = row.get("file_a");
            let file_b: String = row.get("file_b");
            let co_count: i64 = row.get("co_count");
            let id_a = format!("file:{}", file_a);
            let id_b = format!("file:{}", file_b);
            upsert_node(GraphNodeRow { id: id_a.clone(), kind: "file".to_string(), label: file_a.clone(), detail: Some(file_a) });
            upsert_node(GraphNodeRow { id: id_b.clone(), kind: "file".to_string(), label: file_b.clone(), detail: Some(file_b) });
            let confidence = (co_count as f64 / 10.0).min(1.0);
            edges.push(GraphEdgeRow {
                id: format!("co_changes_with:{}:{}", id_a, id_b),
                source: id_a,
                target: id_b,
                edge_type: "co_changes_with".to_string(),
                confidence,
                cross_file: true,
            });
        }

        let mut nodes = nodes.into_values().collect::<Vec<_>>();
        nodes.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.label.cmp(&b.label)));

        Ok((nodes, edges))
    }

}
