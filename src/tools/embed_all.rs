use std::sync::Arc;

use futures::stream::{self, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embeddings::{pack_f32, provider_from_env, EmbeddingProvider};
use crate::error::Error;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmbedAllParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Maximum chunk size in characters when splitting long texts.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
}

fn default_chunk_size() -> usize {
    512
}

const MAX_WARNINGS: usize = 20;
const CONCURRENCY: usize = 8;

#[derive(Debug, Serialize)]
pub struct EmbedAllResult {
    pub decisions_embedded: usize,
    pub constraints_embedded: usize,
    pub episodes_embedded: usize,
    pub commits_embedded: usize,
    pub symbols_embedded: usize,
    pub entity_nodes_embedded: usize,
    pub warnings: Vec<String>,
    pub warnings_total: usize,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(store: &Arc<SqliteStore>, params: EmbedAllParams) -> Result<EmbedAllResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!("path does not exist: {}", params.repo_path),
        })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "non-UTF-8 path".into(),
    })?;

    let repo = store.upsert_repository(repo_path_str, None).await?;
    let chunk_size = params.chunk_size.max(64);

    let provider = match provider_from_env() {
        Some(p) => p,
        None => {
            return Ok(EmbedAllResult {
                decisions_embedded: 0,
                constraints_embedded: 0,
                episodes_embedded: 0,
                commits_embedded: 0,
                symbols_embedded: 0,
                entity_nodes_embedded: 0,
                warnings: vec![
                    "No embedding provider configured (ARCH_MCP_EMBEDDING_PROVIDER not set). \
                     Set it to 'lmstudio', 'ollama', or 'openai' before running embed_all."
                        .to_string(),
                ],
                warnings_total: 1,
            });
        }
    };

    // Wrap in Arc so concurrent futures can share ownership without cloning the trait object.
    let provider: Arc<dyn EmbeddingProvider> = Arc::from(provider);

    let mut warnings = Vec::new();
    let mut decisions_embedded = 0usize;
    let mut constraints_embedded = 0usize;
    let mut episodes_embedded = 0usize;
    let mut commits_embedded = 0usize;
    let mut symbols_embedded = 0usize;
    let mut entity_nodes_embedded = 0usize;

    // Decisions
    let items = store.fetch_decisions_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, text)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                match provider.embed_chunked(&text, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_decision_embedding(id, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("decision {id}: embedding was empty")),
                    Err(e) => Err(format!("decision {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => decisions_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    // Constraints
    let items = store.fetch_constraints_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, text)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                match provider.embed_chunked(&text, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_constraint_embedding(id, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("constraint {id}: embedding was empty")),
                    Err(e) => Err(format!("constraint {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => constraints_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    // Episodes
    let items = store.fetch_episodes_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, content)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                match provider.embed_chunked(&content, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_episode_embedding(id, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("episode {id}: embedding was empty")),
                    Err(e) => Err(format!("episode {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => episodes_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    // Commits
    let items = store.fetch_commits_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, message)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                if message.trim().is_empty() {
                    return Ok(false);
                }
                let id_str = id.to_string();
                match provider.embed_chunked(&message, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_commit_embedding(&id_str, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("commit {id}: embedding was empty")),
                    Err(e) => Err(format!("commit {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => commits_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    // Symbols
    let items = store.fetch_symbols_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, name)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                if name.trim().is_empty() {
                    return Ok(false);
                }
                match provider.embed_chunked(&name, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_symbol_embedding(id, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("symbol {id}: embedding was empty")),
                    Err(e) => Err(format!("symbol {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => symbols_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    // Entity nodes
    let items = store.fetch_entity_nodes_without_embeddings(repo.id).await?;
    let outcomes: Vec<Result<bool, String>> = stream::iter(items)
        .map(|(id, name): (Uuid, String)| {
            let store = store.clone();
            let provider = provider.clone();
            async move {
                if name.trim().is_empty() {
                    return Ok(false);
                }
                match provider.embed_chunked(&name, chunk_size).await {
                    Ok(vec) if !vec.is_empty() => {
                        let blob = pack_f32(&vec);
                        Ok(store.update_entity_node_embedding(id, &blob).await.is_ok())
                    }
                    Ok(_) => Err(format!("entity_node {id}: embedding was empty")),
                    Err(e) => Err(format!("entity_node {id}: {e}")),
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    for o in outcomes {
        match o {
            Ok(true) => entity_nodes_embedded += 1,
            Err(w) => warnings.push(w),
            _ => {}
        }
    }

    let warnings_total = warnings.len();
    warnings.truncate(MAX_WARNINGS);

    crate::tools::freshness::record_lane(store, repo.id, &repo_path, "embedding", "ok").await;

    Ok(EmbedAllResult {
        decisions_embedded,
        constraints_embedded,
        episodes_embedded,
        commits_embedded,
        symbols_embedded,
        entity_nodes_embedded,
        warnings,
        warnings_total,
    })
}
