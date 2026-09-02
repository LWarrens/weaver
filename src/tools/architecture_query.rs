use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::domain::entities::{ArchResponse, TemporalContext};
use crate::embeddings::provider_from_env;
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArchQueryParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Natural-language or keyword query, e.g. "authentication" or "SQLite storage".
    pub query: String,
    /// ISO-8601 timestamp to query as-of. Defaults to current time.
    #[serde(default)]
    pub valid_at: Option<String>,
    /// Maximum number of decisions to return.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Temporal graph expansion depth. Defaults to one hop.
    #[serde(default = "default_graph_depth")]
    pub graph_depth: usize,
    /// Minimum decision confidence to include.
    #[serde(default)]
    pub min_confidence: f64,
    /// Return full decision and constraint text. Defaults to false: text is
    /// truncated to a 280-char snippet. Set true when you need the full ADR body.
    #[serde(default)]
    pub include_full_text: bool,
    /// Freshness verification mode for the evidence behind the returned
    /// decisions: `cached` (default), `fresh`, `skip`, or `strict`.
    #[serde(default)]
    pub verify: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(store: &Arc<SqliteStore>, params: ArchQueryParams) -> Result<ArchResponse, Error> {
    let now = Utc::now().to_rfc3339();

    // --- Validation ---------------------------------------------------------
    if params.query.trim().is_empty() {
        return Err(Error::InvalidInput {
            field: "query",
            reason: "query must not be empty".to_string(),
        });
    }

    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!(
                "path does not exist or is not accessible: {}",
                params.repo_path
            ),
        })?;

    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    if let Some(ref ts) = params.valid_at {
        chrono::DateTime::parse_from_rfc3339(ts).map_err(|_| Error::InvalidInput {
            field: "valid_at",
            reason: format!("not a valid ISO-8601 timestamp: {}", ts),
        })?;
    }

    let valid_at = params.valid_at.as_deref();
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let mut response = ArchResponse::empty();
    response.temporal_context = TemporalContext {
        valid_at: params.valid_at.clone(),
        ingested_at: Some(now.clone()),
    };

    if let Some(w) = crate::tools::freshness::stale_index_warning(&store, repo.id, &repo_path).await {
        response.warnings.push(w);
    }

    let top_k = params.top_k.max(1);
    let graph_depth = params.graph_depth.min(2);
    let min_confidence = params.min_confidence.clamp(0.0, 1.0);

    // --- Search -------------------------------------------------------------
    let keyword_decisions = store
        .search_decisions_ranked(repo.id, &params.query, valid_at, min_confidence)
        .await?;

    let query_embedding: Option<Vec<f32>> = if let Some(provider) = provider_from_env() {
        provider.embed_chunked(&params.query, 512).await.ok()
    } else {
        None
    };
    let semantic_decisions = store
        .semantic_decisions_if_available(
            repo.id,
            query_embedding.as_deref(),
            valid_at,
            min_confidence,
        )
        .await?;


    let seed_ids = keyword_decisions
        .iter()
        .chain(semantic_decisions.as_deref().unwrap_or(&[]).iter())
        .take(5)
        .map(|d| d.id.clone())
        .collect::<Vec<_>>();
    let graph_decisions = store
        .graph_neighbor_decisions(repo.id, &seed_ids, graph_depth, valid_at, min_confidence)
        .await?;
    let decisions = rrf_merge(
        &keyword_decisions,
        semantic_decisions.as_deref().unwrap_or(&[]),
        &graph_decisions,
        top_k,
    );

    if semantic_decisions.is_none() {
        response.warnings.push(
            "Semantic retrieval skipped: no decision embedding provider is configured.".to_string(),
        );
    }
    if params.graph_depth > graph_depth {
        response
            .warnings
            .push("graph_depth was capped at 2 to keep query latency bounded.".to_string());
    }

    if decisions.is_empty() {
        response.answer = Some(format!(
            "No decisions found matching '{}'. Try syncing ADRs first with sync_adrs_from_git.",
            params.query
        ));
        return Ok(response);
    }

    // Fetch constraints for all matched decisions
    let decision_ids: Vec<String> = decisions.iter().map(|d| d.id.clone()).collect();
    let constraints = store.find_constraints_for_decisions(&decision_ids).await?;

    response.answer = Some(format!(
        "Found {} decision(s) matching '{}'.",
        decisions.len(),
        params.query
    ));
    response.confidence = decisions
        .iter()
        .map(|d| d.confidence)
        .fold(f64::INFINITY, f64::min);
    if response.confidence == f64::INFINITY {
        response.confidence = 0.0;
    }
    response.decisions = decisions;
    response.constraints = constraints;

    if !params.include_full_text {
        truncate_response_text(&mut response);
    }

    crate::tools::freshness::attach_manifest(
        &store,
        repo.id,
        &repo_path,
        "query",
        &format!("{}|{}", params.query, params.valid_at.as_deref().unwrap_or("")),
        &mut response,
        crate::tools::freshness::VerifyMode::parse(params.verify.as_deref()),
    )
    .await?;

    Ok(response)
}

pub(crate) fn truncate_response_text(response: &mut crate::domain::entities::ArchResponse) {
    for d in &mut response.decisions {
        d.text = snippet(&d.text, 280);
    }
    for c in &mut response.constraints {
        c.text = snippet(&c.text, 160);
    }
}

fn snippet(s: &str, max_chars: usize) -> String {
    let mut chars = s.char_indices();
    match chars.nth(max_chars) {
        None => s.to_string(),
        Some((byte_pos, _)) => format!("{}…", &s[..byte_pos]),
    }
}

fn default_top_k() -> usize {
    10
}

fn default_graph_depth() -> usize {
    1
}

fn rrf_merge(
    keyword_decisions: &[crate::domain::entities::DecisionSummary],
    semantic_decisions: &[crate::domain::entities::DecisionSummary],
    graph_decisions: &[crate::domain::entities::DecisionSummary],
    top_k: usize,
) -> Vec<crate::domain::entities::DecisionSummary> {
    const RRF_K: f64 = 60.0;

    let mut scores = std::collections::HashMap::<String, f64>::new();
    let mut decisions =
        std::collections::HashMap::<String, crate::domain::entities::DecisionSummary>::new();

    for ranked_list in [keyword_decisions, semantic_decisions, graph_decisions] {
        for (index, decision) in ranked_list.iter().enumerate() {
            decisions
                .entry(decision.id.clone())
                .or_insert_with(|| decision.clone());
            *scores.entry(decision.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + index as f64 + 1.0);
        }
    }

    let mut ranked = decisions.into_values().collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        let b_score = scores.get(&b.id).copied().unwrap_or_default();
        let a_score = scores.get(&a.id).copied().unwrap_or_default();
        b_score
            .total_cmp(&a_score)
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| a.title.cmp(&b.title))
    });
    ranked.truncate(top_k);
    ranked
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_adrs::{run as sync_adrs, SyncAdrsFromGitParams};

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    const SQLITE_ADR: &str = r#"# ADR-0001 Use SQLite for local storage

## Status

Accepted. Date: 2024-01-15.

## Context

We need a lightweight embedded database.

## Decision

We will use SQLite via sqlx for all persistent storage. The module `src/storage/db.rs`
must implement the repository interface. All queries must use parameterized statements.

## Consequences

- Queries must never use string interpolation.
"#;

    const PERSISTENCE_ADR: &str = r#"# ADR-0002 Keep persistence portable

## Status

Accepted. Date: 2024-01-16.

## Context

The application writes architecture memory locally.

## Decision

The persistence layer must stay behind repository methods so callers do not depend
on concrete storage details.

## Consequences

- Query tools use storage helpers instead of building SQL in tool modules.
"#;

    const CONSTRAINTS_ADR: &str = r#"# ADR-0003 Preserve query constraints

## Status

Accepted. Date: 2024-01-17.

## Context

Search tools return architectural knowledge to agents.

## Decision

Query retrieval must include constraints attached to returned decisions.

## Consequences

- Ranking changes must not drop decision constraints.
"#;

    fn summary(id: &str, title: &str) -> crate::domain::entities::DecisionSummary {
        crate::domain::entities::DecisionSummary {
            id: id.to_string(),
            adr_id: id.to_string(),
            episode_id: None,
            title: title.to_string(),
            status: "accepted".to_string(),
            text: title.to_string(),
            valid_from: "2024-01-01T00:00:00Z".to_string(),
            valid_to: None,
            confidence: 1.0,
        }
    }

    #[tokio::test]
    async fn query_returns_matching_decision() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let result = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "sqlite".to_string(),
                valid_at: None,
                top_k: 10,
                graph_depth: 1,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("query");

        assert!(
            !result.decisions.is_empty(),
            "expected at least one decision for query 'sqlite'; warnings: {:?}",
            result.warnings
        );
        assert_eq!(result.decisions[0].adr_id, "ADR-0001");
    }

    #[tokio::test]
    async fn query_returns_empty_for_no_match() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let result = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "kubernetes".to_string(),
                valid_at: None,
                top_k: 10,
                graph_depth: 1,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("query");

        assert!(
            result.decisions.is_empty(),
            "expected no match for 'kubernetes'"
        );
    }

    /// Verify that cosine-similarity search returns decisions when the stored
    /// embedding closely matches the query embedding.
    ///
    /// We bypass the HTTP embedding provider entirely by writing pre-computed
    /// embeddings directly into the DB and then calling
    /// `fetch_decisions_with_embeddings` + the cosine helpers.
    #[tokio::test]
    async fn cosine_search_returns_similar_decisions() {
        use crate::embeddings::{cosine_similarity, pack_f32, unpack_f32};

        // Two identical 3-d unit vectors → cosine = 1.0
        let v: Vec<f32> = vec![0.6, 0.8, 0.0];
        let blob = pack_f32(&v);
        let recovered = unpack_f32(&blob);
        let sim = cosine_similarity(&v, &recovered);
        assert!(
            (sim - 1.0).abs() < 1e-5,
            "cosine of identical vectors should be 1.0, got {}",
            sim
        );

        // Two orthogonal vectors → cosine = 0.0
        let a: Vec<f32> = vec![1.0, 0.0];
        let b: Vec<f32> = vec![0.0, 1.0];
        assert!(
            cosine_similarity(&a, &b).abs() < 1e-6,
            "orthogonal vectors should have cosine 0"
        );

        // Simulate the storage path: sync an ADR, manually write an embedding,
        // then verify fetch_decisions_with_embeddings returns it.
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let repo_path = dir.path().canonicalize().expect("canonicalize");
        let repo_path_str = repo_path.to_str().unwrap();
        let repo = store.upsert_repository(repo_path_str, None).await.expect("repo");

        // Fetch the raw decisions, write embeddings for each
        let pairs = store
            .fetch_decisions_with_embeddings(repo.id, None)
            .await
            .expect("fetch embeddings");
        assert!(!pairs.is_empty(), "no decisions found after sync");

        let target_vec: Vec<f32> = vec![0.5, 0.5, 0.0];
        let target_blob = pack_f32(&target_vec);

        for (summary, _) in &pairs {
            let id: uuid::Uuid = summary.id.parse().expect("decision UUID");
            store
                .update_decision_embedding(id, &target_blob)
                .await
                .expect("update embedding");
        }

        // Now cosine-search with a near-identical query vector
        let query_vec: Vec<f32> = vec![0.49, 0.51, 0.0];
        let all = store
            .fetch_decisions_with_embeddings(repo.id, None)
            .await
            .expect("fetch embeddings 2");

        const COSINE_THRESHOLD: f32 = 0.30;
        let matched: Vec<_> = all
            .into_iter()
            .filter_map(|(summary, blob)| {
                let blob = blob?;
                let vec = unpack_f32(&blob);
                let sim = cosine_similarity(&query_vec, &vec);
                if sim >= COSINE_THRESHOLD { Some(summary) } else { None }
            })
            .collect();

        assert!(
            !matched.is_empty(),
            "cosine search should return at least one decision"
        );
    }

    #[tokio::test]
    async fn query_expands_to_temporal_edge_neighbor() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write sqlite");
        std::fs::write(dir.path().join("0002-persistence.md"), PERSISTENCE_ADR)
            .expect("write persistence");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let repo = store
            .upsert_repository(dir.path().canonicalize().unwrap().to_str().unwrap(), None)
            .await
            .expect("repo");
        let sqlite_decision = store
            .search_decisions(repo.id, "sqlite", None)
            .await
            .expect("sqlite decision")
            .remove(0);
        let persistence_decision = store
            .search_decisions(repo.id, "portable", None)
            .await
            .expect("persistence decision")
            .remove(0);

        store
            .insert_test_temporal_edge(
                "conflicts_with",
                &sqlite_decision.id,
                &persistence_decision.id,
                "2024-01-16T00:00:00Z",
                "2024-01-16T00:00:00Z",
            )
            .await
            .expect("edge");

        let result = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "sqlite".to_string(),
                valid_at: None,
                top_k: 10,
                graph_depth: 1,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("query");

        let adr_ids = result
            .decisions
            .iter()
            .map(|d| d.adr_id.as_str())
            .collect::<Vec<_>>();
        assert!(adr_ids.contains(&"ADR-0001"));
        assert!(
            adr_ids.contains(&"ADR-0002"),
            "expected BFS expansion to include linked decision; got {:?}",
            adr_ids
        );
    }

    #[tokio::test]
    async fn query_respects_top_k() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write sqlite");
        std::fs::write(dir.path().join("0002-persistence.md"), PERSISTENCE_ADR)
            .expect("write persistence");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let result = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "storage persistence".to_string(),
                valid_at: None,
                top_k: 1,
                graph_depth: 1,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("query");

        assert_eq!(result.decisions.len(), 1);
    }

    #[tokio::test]
    async fn query_graph_depth_reaches_second_hop_neighbor() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-sqlite.md"), SQLITE_ADR).expect("write sqlite");
        std::fs::write(dir.path().join("0002-persistence.md"), PERSISTENCE_ADR)
            .expect("write persistence");
        std::fs::write(dir.path().join("0003-constraints.md"), CONSTRAINTS_ADR)
            .expect("write constraints");

        sync_adrs(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let repo = store
            .upsert_repository(dir.path().canonicalize().unwrap().to_str().unwrap(), None)
            .await
            .expect("repo");
        let sqlite_decision = store
            .search_decisions(repo.id, "sqlite", None)
            .await
            .expect("sqlite decision")
            .remove(0);
        let persistence_decision = store
            .search_decisions(repo.id, "portable", None)
            .await
            .expect("persistence decision")
            .remove(0);
        let constraints_decision = store
            .search_decisions(repo.id, "constraints", None)
            .await
            .expect("constraints decision")
            .remove(0);

        store
            .insert_test_temporal_edge(
                "supports",
                &sqlite_decision.id,
                &persistence_decision.id,
                "2024-01-16T00:00:00Z",
                "2024-01-16T00:00:00Z",
            )
            .await
            .expect("first edge");
        store
            .insert_test_temporal_edge(
                "supports",
                &persistence_decision.id,
                &constraints_decision.id,
                "2024-01-17T00:00:00Z",
                "2024-01-17T00:00:00Z",
            )
            .await
            .expect("second edge");

        let one_hop = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "sqlite".to_string(),
                valid_at: None,
                top_k: 10,
                graph_depth: 1,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("one hop query");
        let two_hop = run(
            &store,
            ArchQueryParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                query: "sqlite".to_string(),
                valid_at: None,
                top_k: 10,
                graph_depth: 2,
                min_confidence: 0.0,
                include_full_text: true,
                verify: None,
            },
        )
        .await
        .expect("two hop query");

        assert!(!one_hop.decisions.iter().any(|d| d.adr_id == "ADR-0003"));
        assert!(two_hop.decisions.iter().any(|d| d.adr_id == "ADR-0003"));
    }

    #[test]
    fn rrf_boosts_decisions_seen_by_multiple_retrievers() {
        let keyword = vec![summary("A", "keyword only"), summary("B", "both")];
        let semantic = vec![summary("B", "both")];
        let graph = Vec::new();

        let ranked = rrf_merge(&keyword, &semantic, &graph, 10);

        assert_eq!(ranked[0].id, "B");
    }
}
