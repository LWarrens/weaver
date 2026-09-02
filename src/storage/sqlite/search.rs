//! Keyword, semantic, and graph retrieval over decisions.
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
    // Full-text search across decisions
    // -----------------------------------------------------------------------

    /// Search decisions whose text or ADR title contains any of the query terms.
    /// Terms are words (≥3 chars) extracted from `query`. Returns distinct decisions
    /// ordered by confidence descending.
    pub async fn search_decisions(
        &self,
        repo_id: Uuid,
        query: &str,
        valid_at: Option<&str>,
    ) -> Result<Vec<DecisionSummary>, Error> {
        self.search_decisions_ranked(repo_id, query, valid_at, 0.0)
            .await
    }

    pub async fn search_decisions_ranked(
        &self,
        repo_id: Uuid,
        query: &str,
        valid_at: Option<&str>,
        min_confidence: f64,
    ) -> Result<Vec<DecisionSummary>, Error> {
        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");

        // Extract meaningful words from the query string
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 3)
            .map(|w| w.to_lowercase())
            .collect();

        if terms.is_empty() {
            return Ok(vec![]);
        }

        // Build a WHERE clause: (LOWER(d.text) LIKE ? OR LOWER(display title) LIKE ?) for each term
        let term_clause = terms
            .iter()
            .map(|_| "(LOWER(d.text) LIKE ? OR LOWER(COALESCE(d.title, a.title, e.source)) LIKE ?)")
            .collect::<Vec<_>>()
            .join(" OR ");

        let sql = format!(
            r#"SELECT DISTINCT d.id, d.text, d.valid_from, d.valid_to, d.confidence,
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
                 AND d.confidence >= ?
                 AND ({})
               ORDER BY d.confidence DESC, title ASC"#,
            term_clause
        );

        let mut q = sqlx::query(&sql);
        let repo_id_str = repo_id.to_string();
        q = q
            .bind(&repo_id_str)
            .bind(&repo_id_str)
            .bind(at)
            .bind(at)
            .bind(min_confidence);
        for term in &terms {
            let like_term = format!("%{}%", term);
            q = q.bind(like_term.clone()).bind(like_term);
        }

        let rows = q.fetch_all(&self.pool).await?;

        let mut decisions = rows
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
            .collect::<Vec<_>>();

        decisions.sort_by(|a, b| {
            let b_score = term_frequency_score(b, &terms);
            let a_score = term_frequency_score(a, &terms);
            b_score
                .cmp(&a_score)
                .then_with(|| b.confidence.total_cmp(&a.confidence))
                .then_with(|| a.title.cmp(&b.title))
        });

        Ok(decisions)
    }

    pub async fn graph_neighbor_decisions(
        &self,
        repo_id: Uuid,
        seed_decision_ids: &[String],
        depth: usize,
        valid_at: Option<&str>,
        min_confidence: f64,
    ) -> Result<Vec<DecisionSummary>, Error> {
        if seed_decision_ids.is_empty() || depth == 0 {
            return Ok(vec![]);
        }

        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");
        let mut visited = seed_decision_ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut frontier = visited.iter().cloned().collect::<Vec<_>>();
        let mut discovered = Vec::new();

        for _ in 0..depth {
            if frontier.is_empty() {
                break;
            }

            let frontier_clause = frontier.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let edge_type_clause = QUERY_EDGE_TYPES
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT source_id, target_id
                   FROM temporal_edges
                   WHERE edge_type IN ({})
                     AND source_type = 'decision'
                     AND target_type = 'decision'
                     AND valid_from <= ?
                     AND (valid_to IS NULL OR valid_to > ?)
                     AND (source_id IN ({}) OR target_id IN ({}))"#,
                edge_type_clause, frontier_clause, frontier_clause
            );

            let mut q = sqlx::query(&sql);
            for edge_type in QUERY_EDGE_TYPES {
                q = q.bind(edge_type);
            }
            q = q.bind(at).bind(at);
            for id in &frontier {
                q = q.bind(id);
            }
            for id in &frontier {
                q = q.bind(id);
            }

            let rows = q.fetch_all(&self.pool).await?;
            let mut next_frontier = Vec::new();
            for row in rows {
                let source_id: String = row.get("source_id");
                let target_id: String = row.get("target_id");
                for id in [source_id, target_id] {
                    if visited.insert(id.clone()) {
                        next_frontier.push(id.clone());
                        discovered.push(id);
                    }
                }
            }

            // Commit-bridged hop: decisions evidenced by the same commit as a
            // frontier decision (`evidences` Commit → Decision edges emitted by
            // sync_commits_from_git) count as one-hop neighbours.
            let bridge_sql = format!(
                r#"SELECT e2.target_id AS neighbour_id
                   FROM temporal_edges e1
                   JOIN temporal_edges e2
                     ON e1.source_id = e2.source_id
                   WHERE e1.edge_type = 'evidences' AND e2.edge_type = 'evidences'
                     AND e1.source_type = 'commit' AND e2.source_type = 'commit'
                     AND e1.target_type = 'decision' AND e2.target_type = 'decision'
                     AND e1.valid_from <= ? AND (e1.valid_to IS NULL OR e1.valid_to > ?)
                     AND e2.valid_from <= ? AND (e2.valid_to IS NULL OR e2.valid_to > ?)
                     AND e1.target_id IN ({})
                     AND e2.target_id NOT IN ({})"#,
                frontier_clause, frontier_clause
            );
            let mut bq = sqlx::query(&bridge_sql).bind(at).bind(at).bind(at).bind(at);
            for id in &frontier {
                bq = bq.bind(id);
            }
            for id in &frontier {
                bq = bq.bind(id);
            }
            for row in bq.fetch_all(&self.pool).await? {
                let id: String = row.get("neighbour_id");
                if visited.insert(id.clone()) {
                    next_frontier.push(id.clone());
                    discovered.push(id);
                }
            }

            frontier = next_frontier;
        }

        self.find_decisions_by_ids(repo_id, &discovered, valid_at, min_confidence)
            .await
    }

    /// Semantic (cosine-similarity) search over decision embeddings.
    /// Pass a pre-computed query embedding; returns `None` when no embedding was available.
    pub async fn semantic_decisions_if_available(
        &self,
        repo_id: Uuid,
        query_embedding: Option<&[f32]>,
        valid_at: Option<&str>,
        min_confidence: f64,
    ) -> Result<Option<Vec<DecisionSummary>>, Error> {
        use crate::embeddings::{cosine_similarity, unpack_f32};

        let query_vec = match query_embedding {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };

        const COSINE_THRESHOLD: f32 = 0.30;

        let pairs = self
            .fetch_decisions_with_embeddings(repo_id, valid_at)
            .await?;

        let mut scored: Vec<(f32, DecisionSummary)> = pairs
            .into_iter()
            .filter_map(|(summary, blob)| {
                if summary.confidence < min_confidence {
                    return None;
                }
                let blob = blob?;
                let vec = unpack_f32(&blob);
                if vec.is_empty() {
                    return None;
                }
                let sim = cosine_similarity(query_vec, &vec);
                if sim >= COSINE_THRESHOLD {
                    Some((sim, summary))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(Some(scored.into_iter().map(|(_, d)| d).collect()))
    }

    pub(crate) async fn find_decisions_by_ids(
        &self,
        repo_id: Uuid,
        decision_ids: &[String],
        valid_at: Option<&str>,
        min_confidence: f64,
    ) -> Result<Vec<DecisionSummary>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }

        let at = valid_at.unwrap_or("9999-12-31T23:59:59Z");
        let placeholders = decision_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            r#"SELECT d.id, d.text, d.valid_from, d.valid_to, d.confidence,
                      COALESCE(a.adr_id, 'episode:' || e.id) AS adr_id,
                      d.episode_id,
                      COALESCE(d.title, a.title, e.source) AS title,
                      COALESCE(a.status, 'episode') AS status
               FROM decisions d
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               LEFT JOIN episodes e ON e.id = d.episode_id
               WHERE d.id IN ({})
                 AND (a.repo_id = ? OR e.repo_id = ?)
                 AND d.valid_from <= ?
                 AND (d.valid_to IS NULL OR d.valid_to > ?)
                 AND d.confidence >= ?"#,
            placeholders
        );

        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id);
        }
        let repo_id_str = repo_id.to_string();
        let rows = q
            .bind(&repo_id_str)
            .bind(&repo_id_str)
            .bind(at)
            .bind(at)
            .bind(min_confidence)
            .fetch_all(&self.pool)
            .await?;

        let mut decisions = rows
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
            .collect::<Vec<_>>();

        let positions = decision_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<std::collections::HashMap<_, _>>();
        decisions.sort_by_key(|d| positions.get(d.id.as_str()).copied().unwrap_or(usize::MAX));
        Ok(decisions)
    }

}
