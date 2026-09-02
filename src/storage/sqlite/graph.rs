//! Graph snapshots, schema introspection, and index status.
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
    // Focused (neighbourhood) graph snapshot
    // -----------------------------------------------------------------------

    /// Return the subgraph within `depth` hops of any symbol matching `focus`.
    /// Exact name match first; falls back to substring (LIKE %focus%).
    /// No edge-count cap — the neighbourhood is always complete.
    pub async fn fetch_focused_snapshot(
        &self,
        repo_id: Uuid,
        focus: &str,
        depth: u32,
        valid_at: &str,
    ) -> Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>), Error> {
        use std::collections::{HashMap, HashSet, VecDeque};

        // ---- Step 1: find seed symbol IDs ----
        let seed_rows = sqlx::query(
            r#"SELECT s.id FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.name = ? AND s.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .bind(focus)
        .fetch_all(&self.pool)
        .await?;

        let mut seeds: Vec<String> = seed_rows
            .iter()
            .map(|r| r.get::<String, _>("id"))
            .collect();

        if seeds.is_empty() {
            let like_pat = format!("%{}%", focus);
            let rows = sqlx::query(
                r#"SELECT s.id FROM symbols s
                   JOIN files f ON f.id = s.file_id
                   WHERE f.repo_id = ? AND s.name LIKE ? AND s.valid_to IS NULL
                   LIMIT 30"#,
            )
            .bind(repo_id.to_string())
            .bind(like_pat)
            .fetch_all(&self.pool)
            .await?;
            seeds = rows.iter().map(|r| r.get::<String, _>("id")).collect();
        }

        if seeds.is_empty() {
            return Ok((vec![], vec![]));
        }

        // ---- Step 2: load all live symbol edges into memory, build adjacency ----
        struct EdgeRec {
            edge_id: String,
            from_id: String,
            to_id: String,
            edge_type: String,
            confidence: f64,
            from_file: String,
            to_file: String,
        }

        let raw_edges = sqlx::query(
            r#"SELECT se.id AS edge_id, se.from_id, se.to_id, se.edge_type, se.confidence,
                      from_file.path AS from_file, COALESCE(to_file.path, '') AS to_file
               FROM symbol_edges se
               JOIN symbols from_sym ON from_sym.id = se.from_id
               JOIN files from_file ON from_file.id = from_sym.file_id
               LEFT JOIN symbols to_sym ON to_sym.id = se.to_id
               LEFT JOIN files to_file ON to_file.id = to_sym.file_id
               WHERE se.repo_id = ?
                 AND se.valid_from <= ?
                 AND (se.valid_to IS NULL OR se.valid_to > ?)
                 AND se.to_id IS NOT NULL
               LIMIT 100000"#,
        )
        .bind(repo_id.to_string())
        .bind(valid_at)
        .bind(valid_at)
        .fetch_all(&self.pool)
        .await?;

        let mut edge_recs: Vec<EdgeRec> = Vec::with_capacity(raw_edges.len());
        let mut adj_out: HashMap<String, Vec<usize>> = HashMap::new();
        let mut adj_in: HashMap<String, Vec<usize>> = HashMap::new();

        for row in raw_edges {
            let idx = edge_recs.len();
            let from_id: String = row.get("from_id");
            let to_id: String = row.get("to_id");
            adj_out.entry(from_id.clone()).or_default().push(idx);
            adj_in.entry(to_id.clone()).or_default().push(idx);
            edge_recs.push(EdgeRec {
                edge_id: row.get("edge_id"),
                from_id,
                to_id,
                edge_type: row.get("edge_type"),
                confidence: row.get("confidence"),
                from_file: row.get("from_file"),
                to_file: row.get("to_file"),
            });
        }

        // ---- Step 3: BFS up to `depth` hops (both directions) ----
        let mut visited: HashSet<String> = seeds.iter().cloned().collect();
        let mut queue: VecDeque<(String, u32)> =
            seeds.into_iter().map(|id| (id, 0)).collect();
        let mut included_edges: HashSet<usize> = HashSet::new();

        while let Some((sym_id, hop)) = queue.pop_front() {
            // Outbound
            for &idx in adj_out.get(&sym_id).map(|v| v.as_slice()).unwrap_or(&[]) {
                included_edges.insert(idx);
                if hop < depth {
                    let nb = &edge_recs[idx].to_id;
                    if visited.insert(nb.clone()) {
                        queue.push_back((nb.clone(), hop + 1));
                    }
                }
            }
            // Inbound
            for &idx in adj_in.get(&sym_id).map(|v| v.as_slice()).unwrap_or(&[]) {
                included_edges.insert(idx);
                if hop < depth {
                    let nb = &edge_recs[idx].from_id;
                    if visited.insert(nb.clone()) {
                        queue.push_back((nb.clone(), hop + 1));
                    }
                }
            }
        }

        // ---- Step 4: fetch symbol details for all visited IDs ----
        let sym_detail_rows = sqlx::query(
            r#"SELECT s.id, s.name, s.kind, f.path AS file_path
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut sym_detail: HashMap<String, (String, String, String)> = HashMap::new();
        for row in sym_detail_rows {
            let id: String = row.get("id");
            if visited.contains(&id) {
                sym_detail.insert(
                    id,
                    (
                        row.get("name"),
                        row.get("kind"),
                        row.get("file_path"),
                    ),
                );
            }
        }

        // ---- Step 5: build output ----
        let mut nodes: HashMap<String, GraphNodeRow> = HashMap::new();
        let mut edges: Vec<GraphEdgeRow> = Vec::new();

        let mut upsert_node = |n: GraphNodeRow| {
            nodes.entry(n.id.clone()).or_insert(n);
        };

        // Symbol nodes carry file path as metadata. File nodes are reserved for
        // file-scoped facts such as ADR and commit links.
        for sym_id in &visited {
            if let Some((name, kind, file)) = sym_detail.get(sym_id) {
                upsert_node(GraphNodeRow {
                    id: sym_id.clone(),
                    kind: "symbol".to_string(),
                    label: name.clone(),
                    detail: Some(format!("{} · {}", kind, file)),
                });
            }
        }

        // Call edges
        for idx in &included_edges {
            let e = &edge_recs[*idx];
            edges.push(GraphEdgeRow {
                id: e.edge_id.clone(),
                source: e.from_id.clone(),
                target: e.to_id.clone(),
                edge_type: e.edge_type.clone(),
                confidence: e.confidence,
                cross_file: e.from_file != e.to_file,
            });
        }

        // Decision/constraint links for files in the neighbourhood.
        // Build file_set from sym_detail (not `nodes`) to avoid a mutable/immutable
        // borrow conflict with the upsert_node closure.
        let file_set: HashSet<String> = sym_detail
            .values()
            .map(|(_, _, file)| file.clone())
            .collect();

        if !file_set.is_empty() {
            let code_rows = sqlx::query(
                r#"SELECT dcl.id AS link_id, dcl.decision_id, dcl.file_path, dcl.link_type, dcl.confidence,
                          d.title AS decision_title
                   FROM decision_code_links dcl
                   JOIN decisions d ON d.id = dcl.decision_id
                   JOIN adr_documents ad ON ad.id = d.adr_id
                   WHERE ad.repo_id = ?
                     AND dcl.valid_from <= ?
                     AND (dcl.valid_to IS NULL OR dcl.valid_to > ?)"#,
            )
            .bind(repo_id.to_string())
            .bind(valid_at)
            .bind(valid_at)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

            for row in code_rows {
                let file_path: String = row.get("file_path");
                if !file_set.contains(&file_path) {
                    continue;
                }
                let link_id: String = row.get("link_id");
                let decision_id: String = row.get("decision_id");
                let link_type: String = row.get("link_type");
                let confidence: f64 = row.get("confidence");
                let title: Option<String> = row.get("decision_title");
                let file_id = format!("file:{}", file_path);
                upsert_node(GraphNodeRow {
                    id: file_id.clone(),
                    kind: "file".to_string(),
                    label: file_path.clone(),
                    detail: Some(file_path),
                });
                upsert_node(GraphNodeRow {
                    id: decision_id.clone(),
                    kind: "decision".to_string(),
                    label: title.unwrap_or_else(|| decision_id.clone()),
                    detail: None,
                });
                edges.push(GraphEdgeRow {
                    id: link_id,
                    source: decision_id,
                    target: file_id,
                    edge_type: link_type,
                    confidence,
                    cross_file: false,
                });
            }
        }

        let mut node_list: Vec<GraphNodeRow> = nodes.into_values().collect();
        node_list.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.label.cmp(&b.label)));

        Ok((node_list, edges))
    }

    // ADR lineage helpers
    // -----------------------------------------------------------------------

    /// Find an ADR document by flexible adr_id: accepts "ADR-0003", "ADR-3", "0003", or "3".
    /// Returns the most recently ingested version (including closed/superseded ADRs).
    pub async fn find_adr_by_adr_id_flex(
        &self,
        repo_id: Uuid,
        adr_id: &str,
    ) -> Result<Option<AdrDocument>, Error> {
        // Extract numeric part: strip optional "ADR-" prefix (case-insensitive).
        let s = adr_id.trim();
        let numeric_str = s
            .to_uppercase()
            .strip_prefix("ADR-")
            .map(str::to_string)
            .unwrap_or_else(|| s.to_string());

        let n: i64 = match numeric_str.parse() {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };

        // Integer comparison on the part after '-' (or full adr_id if no '-').
        let row = sqlx::query(
            r#"SELECT id, repo_id, adr_id, title, status, date, context, decision,
                      consequences, supersedes, superseded_by, file_mentions,
                      service_mentions, module_mentions, source_uri,
                      effective_from, effective_to, valid_from, valid_to,
                      ingested_at, source_time, confidence
               FROM adr_documents
               WHERE repo_id = ?
                 AND CAST(SUBSTR(adr_id, INSTR(adr_id, '-') + 1) AS INTEGER) = ?
               ORDER BY valid_from DESC
               LIMIT 1"#,
        )
        .bind(repo_id.to_string())
        .bind(n)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_adr(&r)).transpose()
    }

    /// Walk supersession_edges backwards: return all AdrDocuments that the given
    /// document superseded, up to `max_hops` hops, ordered by discovery (oldest first).
    pub async fn get_supersession_predecessors(
        &self,
        doc_id: Uuid,
        max_hops: usize,
    ) -> Result<Vec<AdrDocument>, Error> {
        let mut result: Vec<AdrDocument> = Vec::new();
        let mut visited: Vec<Uuid> = vec![doc_id];
        let mut frontier: Vec<Uuid> = vec![doc_id];

        for _ in 0..max_hops {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<Uuid> = Vec::new();
            for id in frontier {
                let rows = sqlx::query(
                    r#"SELECT a.id, a.repo_id, a.adr_id, a.title, a.status, a.date, a.context,
                              a.decision, a.consequences, a.supersedes, a.superseded_by,
                              a.file_mentions, a.service_mentions, a.module_mentions, a.source_uri,
                              a.effective_from, a.effective_to, a.valid_from, a.valid_to,
                              a.ingested_at, a.source_time, a.confidence
                       FROM supersession_edges se
                       JOIN adr_documents a ON a.id = se.superseded_id
                       WHERE se.superseder_id = ?"#,
                )
                .bind(id.to_string())
                .fetch_all(&self.pool)
                .await?;

                for row in &rows {
                    let doc = row_to_adr(row)?;
                    if !visited.contains(&doc.id) {
                        visited.push(doc.id);
                        next.push(doc.id);
                        result.push(doc);
                    }
                }
            }
            frontier = next;
        }

        Ok(result)
    }

    /// Walk supersession_edges forwards: return all AdrDocuments that superseded
    /// the given document, up to `max_hops` hops, ordered by discovery (oldest first).
    pub async fn get_supersession_successors(
        &self,
        doc_id: Uuid,
        max_hops: usize,
    ) -> Result<Vec<AdrDocument>, Error> {
        let mut result: Vec<AdrDocument> = Vec::new();
        let mut visited: Vec<Uuid> = vec![doc_id];
        let mut frontier: Vec<Uuid> = vec![doc_id];

        for _ in 0..max_hops {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<Uuid> = Vec::new();
            for id in frontier {
                let rows = sqlx::query(
                    r#"SELECT a.id, a.repo_id, a.adr_id, a.title, a.status, a.date, a.context,
                              a.decision, a.consequences, a.supersedes, a.superseded_by,
                              a.file_mentions, a.service_mentions, a.module_mentions, a.source_uri,
                              a.effective_from, a.effective_to, a.valid_from, a.valid_to,
                              a.ingested_at, a.source_time, a.confidence
                       FROM supersession_edges se
                       JOIN adr_documents a ON a.id = se.superseder_id
                       WHERE se.superseded_id = ?"#,
                )
                .bind(id.to_string())
                .fetch_all(&self.pool)
                .await?;

                for row in &rows {
                    let doc = row_to_adr(row)?;
                    if !visited.contains(&doc.id) {
                        visited.push(doc.id);
                        next.push(doc.id);
                        result.push(doc);
                    }
                }
            }
            frontier = next;
        }

        Ok(result)
    }
}

impl SqliteStore {
    /// Returns (from_id, to_id) pairs for calls/imports edges in the repo (active only, to_id not null).
    pub async fn get_call_edges_for_repo(&self, repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>, Error> {
        let rows = sqlx::query(
            r#"SELECT from_id, to_id FROM symbol_edges
               WHERE repo_id = ? AND to_id IS NOT NULL AND valid_to IS NULL
                 AND edge_type IN ('calls', 'imports')"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let from_str: String = row.get("from_id");
            let to_str: String = row.get("to_id");
            let from = Uuid::parse_str(&from_str).map_err(|e| Error::Parse(e.to_string()))?;
            let to = Uuid::parse_str(&to_str).map_err(|e| Error::Parse(e.to_string()))?;
            out.push((from, to));
        }
        Ok(out)
    }

    /// Returns (symbol_id, symbol_name, file_path) for all active symbols in repo.
    pub async fn get_symbols_with_files_for_repo(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String, String)>, Error> {
        let rows = sqlx::query(
            r#"SELECT s.id, s.name, f.path FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id_str: String = row.get("id");
            let id = Uuid::parse_str(&id_str).map_err(|e| Error::Parse(e.to_string()))?;
            out.push((id, row.get("name"), row.get("path")));
        }
        Ok(out)
    }

    /// Marks all existing communities for a repo as closed (sets valid_to).
    pub async fn close_stale_communities_for_repo(
        &self,
        repo_id: Uuid,
        valid_to: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE communities SET valid_to = ? WHERE repo_id = ? AND valid_to IS NULL",
        )
        .bind(valid_to)
        .bind(repo_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Inserts a community record.
    pub async fn insert_community(
        &self,
        id: Uuid,
        repo_id: Uuid,
        label: &str,
        size: usize,
        valid_from: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO communities (id, repo_id, label, size, valid_from) VALUES (?,?,?,?,?)",
        )
        .bind(id.to_string())
        .bind(repo_id.to_string())
        .bind(label)
        .bind(size as i64)
        .bind(valid_from)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Inserts a community membership link.
    pub async fn insert_community_member(
        &self,
        community_id: Uuid,
        symbol_id: Uuid,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO community_members (community_id, symbol_id) VALUES (?,?)",
        )
        .bind(community_id.to_string())
        .bind(symbol_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns communities with their file paths and member names for a repo.
    /// Returns Vec of (community_id, label, size, Vec<file_path>, Vec<symbol_name>)
    pub async fn get_communities_for_repo(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String, usize, Vec<String>, Vec<String>)>, Error> {
        let rows = sqlx::query(
            r#"SELECT c.id, c.label, c.size, f.path AS file_path, s.name AS sym_name
               FROM communities c
               JOIN community_members cm ON cm.community_id = c.id
               JOIN symbols s ON s.id = cm.symbol_id
               JOIN files f ON f.id = s.file_id
               WHERE c.repo_id = ? AND c.valid_to IS NULL"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        use std::collections::{HashMap, HashSet};
        // community_id -> (label, size, file_paths, sym_names)
        let mut map: HashMap<String, (String, usize, HashSet<String>, HashSet<String>)> =
            HashMap::new();
        for row in &rows {
            let id_str: String = row.get("id");
            let label: String = row.get("label");
            let size: i64 = row.get("size");
            let file_path: String = row.get("file_path");
            let sym_name: String = row.get("sym_name");

            let entry = map
                .entry(id_str)
                .or_insert_with(|| (label, size as usize, HashSet::new(), HashSet::new()));
            entry.2.insert(file_path);
            entry.3.insert(sym_name);
        }

        let mut out = Vec::with_capacity(map.len());
        for (id_str, (label, size, file_paths, sym_names)) in map {
            let id = Uuid::parse_str(&id_str).map_err(|e| Error::Parse(e.to_string()))?;
            out.push((
                id,
                label,
                size,
                file_paths.into_iter().collect::<Vec<_>>(),
                sym_names.into_iter().collect::<Vec<_>>(),
            ));
        }
        Ok(out)
    }

    pub async fn index_status_counts(&self, repo_id: Uuid) -> Result<IndexStatusCounts, Error> {
        let rid = repo_id.to_string();

        let adrs_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM adr_documents WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let adrs_last_ingested_at: Option<String> = sqlx::query_scalar(
            "SELECT MAX(ingested_at) FROM adr_documents WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let decisions_total: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE a.repo_id = ? OR e.repo_id = ?"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let decisions_embedded: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?) AND d.embedding IS NOT NULL"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let decisions_last_ingested_at: Option<String> = sqlx::query_scalar(
            r#"SELECT MAX(d.ingested_at) FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE a.repo_id = ? OR e.repo_id = ?"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let constraints_total: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE a.repo_id = ? OR e.repo_id = ?"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let constraints_embedded: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?) AND c.embedding IS NOT NULL"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let constraints_last_ingested_at: Option<String> = sqlx::query_scalar(
            r#"SELECT MAX(c.ingested_at) FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE a.repo_id = ? OR e.repo_id = ?"#,
        )
        .bind(&rid).bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let episodes_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episodes WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let episodes_embedded: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episodes WHERE repo_id = ? AND embedding IS NOT NULL",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let episodes_last_ingested_at: Option<String> = sqlx::query_scalar(
            "SELECT MAX(ingested_at) FROM episodes WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let commits_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM commits WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let commits_embedded: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM commits WHERE repo_id = ? AND embedding IS NOT NULL",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let commits_last_ingested_at: Option<String> = sqlx::query_scalar(
            "SELECT MAX(ingested_at) FROM commits WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let symbols_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let symbols_embedded: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND s.embedding IS NOT NULL"#,
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let symbols_last_ingested_at: Option<String> = sqlx::query_scalar(
            r#"SELECT MAX(s.ingested_at) FROM symbols s
               JOIN files f ON f.id = s.file_id WHERE f.repo_id = ?"#,
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let files_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM files WHERE repo_id = ? AND valid_to IS NULL",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        let files_last_ingested_at: Option<String> = sqlx::query_scalar(
            "SELECT MAX(ingested_at) FROM files WHERE repo_id = ?",
        )
        .bind(&rid)
        .fetch_one(&self.pool)
        .await?;

        Ok(IndexStatusCounts {
            adrs_total,
            adrs_last_ingested_at,
            decisions_total,
            decisions_embedded,
            decisions_last_ingested_at,
            constraints_total,
            constraints_embedded,
            constraints_last_ingested_at,
            episodes_total,
            episodes_embedded,
            episodes_last_ingested_at,
            commits_total,
            commits_embedded,
            commits_last_ingested_at,
            symbols_total,
            symbols_embedded,
            symbols_last_ingested_at,
            files_total,
            files_last_ingested_at,
        })
    }

    /// Returns (orphaned_file_paths, total_active_files) for a repo.
    /// A file is orphaned if no active decision_code_link references its path.
    pub async fn find_orphaned_files(
        &self,
        repo_id: Uuid,
        path_prefix: Option<&str>,
    ) -> Result<(Vec<String>, i64), Error> {
        let rid = repo_id.to_string();

        let total: i64 = if let Some(prefix) = path_prefix {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM files WHERE repo_id = ? AND valid_to IS NULL AND path LIKE ?",
            )
            .bind(&rid)
            .bind(format!("{prefix}%"))
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM files WHERE repo_id = ? AND valid_to IS NULL",
            )
            .bind(&rid)
            .fetch_one(&self.pool)
            .await?
        };

        let base_sql = r#"
            SELECT f.path FROM files f
            WHERE f.repo_id = ? AND f.valid_to IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM decision_code_links dcl
                  JOIN decisions d ON d.id = dcl.decision_id
                  LEFT JOIN adr_documents a ON a.id = d.adr_id
                  LEFT JOIN episodes e ON e.id = d.episode_id
                  WHERE (a.repo_id = ? OR e.repo_id = ?)
                    AND dcl.file_path = f.path
                    AND dcl.valid_to IS NULL
              )"#;

        let rows: Vec<String> = if let Some(prefix) = path_prefix {
            sqlx::query_scalar(&format!("{base_sql} AND f.path LIKE ? ORDER BY f.path"))
                .bind(&rid)
                .bind(&rid)
                .bind(&rid)
                .bind(format!("{prefix}%"))
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_scalar(&format!("{base_sql} ORDER BY f.path"))
                .bind(&rid)
                .bind(&rid)
                .bind(&rid)
                .fetch_all(&self.pool)
                .await?
        };

        Ok((rows, total))
    }

    /// Returns (orphaned_symbol tuples, total_active_symbols) for a repo.
    /// A symbol is orphaned if no active decision_code_link names it.
    pub async fn find_orphaned_symbols(
        &self,
        repo_id: Uuid,
        path_prefix: Option<&str>,
    ) -> Result<(Vec<(String, String, String, Option<i64>)>, i64), Error> {
        let rid = repo_id.to_string();

        let total: i64 = if let Some(prefix) = path_prefix {
            sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM symbols s
                   JOIN files f ON f.id = s.file_id
                   WHERE f.repo_id = ? AND s.valid_to IS NULL AND f.path LIKE ?"#,
            )
            .bind(&rid)
            .bind(format!("{prefix}%"))
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.repo_id = ? AND s.valid_to IS NULL",
            )
            .bind(&rid)
            .fetch_one(&self.pool)
            .await?
        };

        let base_sql = r#"
            SELECT s.name, s.kind, f.path, s.line
            FROM symbols s
            JOIN files f ON f.id = s.file_id
            WHERE f.repo_id = ? AND s.valid_to IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM decision_code_links dcl
                  WHERE dcl.symbol = s.name AND dcl.valid_to IS NULL
              )"#;

        let rows = if let Some(prefix) = path_prefix {
            sqlx::query(&format!("{base_sql} AND f.path LIKE ? ORDER BY f.path, s.name"))
                .bind(&rid)
                .bind(format!("{prefix}%"))
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query(&format!("{base_sql} ORDER BY f.path, s.name"))
                .bind(&rid)
                .fetch_all(&self.pool)
                .await?
        };

        let result = rows
            .into_iter()
            .map(|row| {
                let name: String = row.get("name");
                let kind: String = row.get("kind");
                let path: String = row.get("path");
                let line: Option<i64> = row.get("line");
                (name, kind, path, line)
            })
            .collect();

        Ok((result, total))
    }

    /// Decisions added/removed in the half-open window [from, to).
    pub async fn diff_decisions_in_range(
        &self,
        repo_id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<(Vec<crate::tools::diff_architecture::DiffEntry>, Vec<crate::tools::diff_architecture::DiffEntry>), Error> {
        let repo_id_str = repo_id.to_string();

        let added_rows = sqlx::query(
            r#"SELECT d.id, COALESCE(d.title, a.title, e.source, '') AS title,
                      a.adr_id, d.valid_from
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_from >= ? AND d.valid_from < ?
               ORDER BY d.valid_from"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let added = added_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_from"),
            })
            .collect();

        let removed_rows = sqlx::query(
            r#"SELECT d.id, COALESCE(d.title, a.title, e.source, '') AS title,
                      a.adr_id, d.valid_to
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_to IS NOT NULL
                 AND d.valid_to >= ? AND d.valid_to < ?
               ORDER BY d.valid_to"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let removed = removed_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_to"),
            })
            .collect();

        Ok((added, removed))
    }

    /// Constraints added/removed in [from, to).
    pub async fn diff_constraints_in_range(
        &self,
        repo_id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<(Vec<crate::tools::diff_architecture::DiffEntry>, Vec<crate::tools::diff_architecture::DiffEntry>), Error> {
        let repo_id_str = repo_id.to_string();

        let added_rows = sqlx::query(
            r#"SELECT c.id, c.text AS title, a.adr_id, c.valid_from
               FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND c.valid_from >= ? AND c.valid_from < ?
               ORDER BY c.valid_from"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let added = added_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_from"),
            })
            .collect();

        let removed_rows = sqlx::query(
            r#"SELECT c.id, c.text AS title, a.adr_id, c.valid_to
               FROM constraints c
               JOIN decisions d ON d.id = c.decision_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE (a.repo_id = ? OR e.repo_id = ?)
                 AND c.valid_to IS NOT NULL
                 AND c.valid_to >= ? AND c.valid_to < ?
               ORDER BY c.valid_to"#,
        )
        .bind(&repo_id_str)
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let removed = removed_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_to"),
            })
            .collect();

        Ok((added, removed))
    }

    /// ADR documents added/removed in [from, to).
    pub async fn diff_adrs_in_range(
        &self,
        repo_id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<(Vec<crate::tools::diff_architecture::DiffEntry>, Vec<crate::tools::diff_architecture::DiffEntry>), Error> {
        let repo_id_str = repo_id.to_string();

        let added_rows = sqlx::query(
            r#"SELECT id, title, adr_id, valid_from
               FROM adr_documents
               WHERE repo_id = ?
                 AND valid_from >= ? AND valid_from < ?
               ORDER BY valid_from"#,
        )
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let added = added_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_from"),
            })
            .collect();

        let removed_rows = sqlx::query(
            r#"SELECT id, title, adr_id, valid_to
               FROM adr_documents
               WHERE repo_id = ?
                 AND valid_to IS NOT NULL
                 AND valid_to >= ? AND valid_to < ?
               ORDER BY valid_to"#,
        )
        .bind(&repo_id_str)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let removed = removed_rows
            .iter()
            .map(|r| crate::tools::diff_architecture::DiffEntry {
                id: r.get("id"),
                title: r.get("title"),
                adr_id: r.get("adr_id"),
                timestamp: r.get("valid_to"),
            })
            .collect();

        Ok((added, removed))
    }

}
