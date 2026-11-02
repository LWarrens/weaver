//! Retroactive decision dedup (entity resolution after the fact).
//!
//! `record_decision_episode` merges an incoming decision into an existing one at
//! ingest time. Decisions ingested before that existed, or before their
//! embeddings did, can still be duplicates. This tool finds them with the same
//! matcher (normalized-exact text, or cosine similarity of the *stored*
//! embeddings) and folds each duplicate into the decision it repeats.
//!
//! It makes no provider calls: a decision without a stored embedding can only
//! match by exact text, and is counted in `decisions_without_embedding` so the
//! caller knows to run `embed_all` and go again.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embeddings::unpack_f32;
use crate::error::Error;
use crate::storage::SqliteStore;
use crate::tools::record_episode::{find_duplicate_decision, normalize_decision_text, KnownDecision};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DedupDecisionsParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Report the merges without applying them. Default true.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Cosine-similarity threshold (0.0-1.0) at or above which two decisions are
    /// duplicates. Default 0.9, matching ingest-time dedup. At 1.0 only
    /// normalized-exact text merges.
    #[serde(default)]
    pub threshold: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct DecisionMerge {
    /// The duplicate, closed by the merge.
    pub loser_id: String,
    pub loser_title: String,
    /// The decision it repeats, which survives.
    pub survivor_id: String,
    pub survivor_title: String,
    pub similarity: f32,
    /// `exact` (normalized text is identical) or `semantic` (embedding similarity).
    pub method: String,
    /// Open constraints carried over to the survivor.
    pub constraints_moved: usize,
    /// Open constraints the survivor already had, closed with the loser.
    pub constraints_duplicate: usize,
}

#[derive(Debug, Serialize)]
pub struct DedupDecisionsResult {
    pub dry_run: bool,
    pub threshold: f32,
    pub decisions_examined: usize,
    /// Merges found; applied unless `dry_run`.
    pub merges: Vec<DecisionMerge>,
    /// Merges actually applied (0 on a dry run).
    pub merged: usize,
    /// Open decisions with no stored embedding: they can only match by exact text.
    pub decisions_without_embedding: usize,
    pub warnings: Vec<String>,
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: DedupDecisionsParams,
) -> Result<DedupDecisionsResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;
    let dry_run = params.dry_run.unwrap_or(true);
    let threshold = params.threshold.unwrap_or(0.9).clamp(0.0, 1.0);
    let repo = store.upsert_repository(repo_path_str, None).await?;
    let mut warnings: Vec<String> = Vec::new();

    let mut all: Vec<KnownDecision> = store
        .fetch_decisions_with_embeddings(repo.id, None)
        .await?
        .into_iter()
        .map(|(summary, blob)| KnownDecision {
            normalized: normalize_decision_text(&summary.text),
            embedding: blob.map(|b| unpack_f32(&b)).filter(|v| !v.is_empty()),
            summary,
        })
        .collect();
    let decisions_examined = all.len();
    let decisions_without_embedding = all.iter().filter(|k| k.embedding.is_none()).count();
    all.sort_by(|a, b| {
        (a.summary.valid_from.as_str(), a.summary.id.as_str())
            .cmp(&(b.summary.valid_from.as_str(), b.summary.id.as_str()))
    });

    // ADR decisions are evidence, never merged away: they are survivors only, and
    // any episode decision may fold into one. Episode decisions then go oldest
    // first, each folding into an earlier kept decision or becoming one.
    let (episode, adr): (Vec<_>, Vec<_>) =
        all.into_iter().partition(|k| k.summary.episode_id.is_some());
    let mut kept: Vec<KnownDecision> = adr;

    // Normalized open-constraint texts per survivor, grown as merges land so two
    // losers folding into one survivor do not both add the same constraint.
    let mut survivor_constraints: HashMap<String, HashSet<String>> = HashMap::new();
    let mut merges: Vec<DecisionMerge> = Vec::new();
    let mut merged = 0usize;

    for loser in episode {
        let Some((idx, similarity)) = find_duplicate_decision(
            &kept,
            &loser.normalized,
            loser.embedding.as_deref(),
            threshold,
        ) else {
            kept.push(loser);
            continue;
        };
        let survivor = &kept[idx];
        let method = if survivor.normalized == loser.normalized { "exact" } else { "semantic" };

        let known = match survivor_constraints.get_mut(&survivor.summary.id) {
            Some(known) => known,
            None => {
                let texts = store
                    .open_constraints_for_decision(&survivor.summary.id)
                    .await?
                    .into_iter()
                    .map(|(_, text)| normalize_decision_text(&text))
                    .collect();
                survivor_constraints.entry(survivor.summary.id.clone()).or_insert(texts)
            }
        };
        let mut move_ids: Vec<String> = Vec::new();
        let mut duplicates = 0usize;
        for (id, text) in store.open_constraints_for_decision(&loser.summary.id).await? {
            if known.insert(normalize_decision_text(&text)) {
                move_ids.push(id);
            } else {
                duplicates += 1;
            }
        }

        if !dry_run {
            let episode_id = loser.summary.episode_id.clone().unwrap_or_default();
            let (loser_id, survivor_id) = (parse_id(&loser.summary.id)?, parse_id(&survivor.summary.id)?);
            store
                .merge_decision_into(
                    loser_id,
                    survivor_id,
                    &episode_id,
                    similarity as f64,
                    &move_ids,
                    &Utc::now().to_rfc3339(),
                )
                .await?;
            merged += 1;
        }

        merges.push(DecisionMerge {
            loser_id: loser.summary.id.clone(),
            loser_title: loser.summary.title.clone(),
            survivor_id: survivor.summary.id.clone(),
            survivor_title: survivor.summary.title.clone(),
            similarity,
            method: method.to_string(),
            constraints_moved: move_ids.len(),
            constraints_duplicate: duplicates,
        });
    }

    if decisions_without_embedding > 0 {
        warnings.push(format!(
            "{decisions_without_embedding} of {decisions_examined} decisions have no stored embedding \
             and can only match by exact text; run embed_all and dedup again to catch semantic duplicates"
        ));
    }
    if dry_run && !merges.is_empty() {
        warnings.push("dry run: nothing was changed; pass dry_run=false to apply".to_string());
    }

    Ok(DedupDecisionsResult {
        dry_run,
        threshold,
        decisions_examined,
        merges,
        merged,
        decisions_without_embedding,
        warnings,
    })
}

fn parse_id(id: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(id).map_err(|e| Error::Other(anyhow::anyhow!("invalid decision id in store: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::{Constraint, Decision, DecisionCodeLink, LinkSource, LinkType};
    use crate::embeddings::pack_f32;
    use crate::storage::sqlite::TemporalEdge;

    const T0: &str = "2026-06-01T09:00:00Z";
    const T1: &str = "2026-06-02T09:00:00Z";

    struct Fixture {
        store: Arc<SqliteStore>,
        repo_id: Uuid,
        repo_path: String,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dunce::canonicalize(dir.path()).unwrap().to_string_lossy().to_string();
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        store.run_migrations().await.unwrap();
        let store = Arc::new(store);
        let repo_id = store.upsert_repository(&repo_path, None).await.unwrap().id;
        Fixture { store, repo_id, repo_path, _dir: dir }
    }

    impl Fixture {
        /// An episode-sourced decision, as ingest left them before dedup existed.
        async fn decision(&self, text: &str, at: &str) -> (Uuid, Uuid) {
            let episode = Uuid::new_v4();
            self.store
                .insert_episode(&episode, self.repo_id, "meeting", None, "notes", at, at)
                .await
                .unwrap();
            let id = Uuid::new_v4();
            self.store
                .insert_decision(&Decision {
                    id,
                    title: Some(format!("decision {}", &id.to_string()[..4])),
                    adr_id: None,
                    episode_id: Some(episode),
                    text: text.to_string(),
                    source_uri: format!("episode:{episode}"),
                    valid_from: at.to_string(),
                    valid_to: None,
                    ingested_at: at.to_string(),
                    source_time: Some(at.to_string()),
                    confidence: 1.0,
                    evidence_refs: vec![],
                })
                .await
                .unwrap();
            (id, episode)
        }

        async fn constraint(&self, decision: Uuid, text: &str) -> Uuid {
            let id = Uuid::new_v4();
            self.store
                .insert_constraint(&Constraint {
                    id,
                    decision_id: decision,
                    text: text.to_string(),
                    source_uri: "episode:x".to_string(),
                    valid_from: T0.to_string(),
                    valid_to: None,
                    ingested_at: T0.to_string(),
                    source_time: None,
                    confidence: 1.0,
                    evidence_refs: vec![],
                })
                .await
                .unwrap();
            self.store
                .insert_temporal_edge(&TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "imposes".to_string(),
                    source_id: decision.to_string(),
                    source_type: "decision".to_string(),
                    target_id: id.to_string(),
                    target_type: "constraint".to_string(),
                    valid_from: T0.to_string(),
                    valid_to: None,
                    ingested_at: T0.to_string(),
                    confidence: 1.0,
                    evidence_refs: vec![],
                })
                .await
                .unwrap();
            id
        }

        async fn link(&self, decision: Uuid, file: &str) {
            self.store
                .insert_decision_code_link(&DecisionCodeLink {
                    id: Uuid::new_v4(),
                    decision_id: decision,
                    file_path: file.to_string(),
                    symbol: None,
                    link_type: LinkType::Mentions,
                    link_source: LinkSource::Inferred,
                    confidence: 1.0,
                    valid_from: T0.to_string(),
                    valid_to: None,
                    ingested_at: T0.to_string(),
                    evidence_refs: vec![],
                })
                .await
                .unwrap();
        }

        async fn embed(&self, decision: Uuid, v: &[f32]) {
            self.store.update_decision_embedding(decision, &pack_f32(v)).await.unwrap();
        }

        async fn dedup(&self, dry_run: bool, threshold: Option<f32>) -> DedupDecisionsResult {
            run(
                &self.store,
                DedupDecisionsParams {
                    repo_path: self.repo_path.clone(),
                    dry_run: Some(dry_run),
                    threshold,
                },
            )
            .await
            .unwrap()
        }

        async fn open_constraints(&self, decision: Uuid) -> Vec<String> {
            let mut texts: Vec<String> = self
                .store
                .open_constraints_for_decision(&decision.to_string())
                .await
                .unwrap()
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            texts.sort();
            texts
        }

        async fn is_open(&self, decision: Uuid) -> bool {
            self.store
                .fetch_decisions_with_embeddings(self.repo_id, None)
                .await
                .unwrap()
                .iter()
                .any(|(d, _)| d.id == decision.to_string())
        }
    }

    #[tokio::test]
    async fn exact_duplicate_folds_into_the_earlier_decision() {
        let f = fixture().await;
        let (d1, _) = f.decision("Use event sourcing for audit history.", T0).await;
        let (d2, _) = f.decision("use  event sourcing for AUDIT history.", T1).await;
        f.constraint(d1, "All writes must emit an event.").await;
        f.constraint(d2, "all writes  must emit an event.").await; // duplicate of d1's
        f.constraint(d2, "Events are immutable.").await; // new
        f.link(d1, "src/a.rs").await;
        f.link(d2, "src/a.rs").await; // duplicate of d1's
        f.link(d2, "src/b.rs").await; // new

        // A dry run reports the merge and changes nothing.
        let dry = f.dedup(true, None).await;
        assert_eq!(dry.merges.len(), 1);
        assert_eq!(dry.merged, 0);
        assert_eq!(dry.merges[0].method, "exact");
        assert_eq!(dry.merges[0].loser_id, d2.to_string());
        assert_eq!(dry.merges[0].survivor_id, d1.to_string());
        assert_eq!((dry.merges[0].constraints_moved, dry.merges[0].constraints_duplicate), (1, 1));
        assert!(f.is_open(d2).await, "dry run must not close anything");

        let applied = f.dedup(false, None).await;
        assert_eq!(applied.merged, 1);
        assert!(f.is_open(d1).await);
        assert!(!f.is_open(d2).await, "the duplicate is soft-closed");

        // The survivor kept its constraint and gained the genuinely new one.
        assert_eq!(
            f.open_constraints(d1).await,
            vec!["All writes must emit an event.".to_string(), "Events are immutable.".to_string()]
        );
        assert!(f.open_constraints(d2).await.is_empty(), "duplicates close with the loser");

        // Code links: the survivor now covers both files, once each.
        let mut files = f.store.file_paths_linked_to_decision(&d1.to_string()).await.unwrap();
        files.sort();
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"]);
        assert!(f.store.file_paths_linked_to_decision(&d2.to_string()).await.unwrap().is_empty());

        // Provenance: the loser's episode now `supports` the survivor.
        assert_eq!(f.store.count_open_temporal_edges_of_type("supports").await.unwrap(), 1);

        // Idempotent: nothing left to merge.
        let again = f.dedup(false, None).await;
        assert!(again.merges.is_empty());
        assert_eq!(f.store.count_open_temporal_edges_of_type("supports").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn semantic_duplicate_uses_stored_embeddings_and_respects_the_threshold() {
        let f = fixture().await;
        let (a, _) = f.decision("Adopt an event-sourced audit log.", T0).await;
        let (b, _) = f.decision("We will keep audit history as a stream of events.", T1).await;
        let (c, _) = f.decision("Deploy to Kubernetes.", T1).await;
        f.embed(a, &[1.0, 0.0, 0.0]).await;
        f.embed(b, &[0.99, 0.1, 0.0]).await; // cosine ~0.995 with a
        f.embed(c, &[0.0, 1.0, 0.0]).await; // orthogonal

        let result = f.dedup(false, Some(0.9)).await;
        assert_eq!(result.merges.len(), 1);
        assert_eq!(result.merges[0].method, "semantic");
        assert!(result.merges[0].similarity > 0.99);
        assert!(f.is_open(a).await && !f.is_open(b).await && f.is_open(c).await);
    }

    #[tokio::test]
    async fn threshold_decides_whether_a_near_duplicate_merges() {
        let f = fixture().await;
        let (a, _) = f.decision("Adopt an event-sourced audit log.", T0).await;
        let (b, _) = f.decision("We will keep audit history as a stream of events.", T1).await;
        f.embed(a, &[1.0, 0.0]).await;
        f.embed(b, &[0.99, 0.1]).await; // cosine ~0.995

        assert!(f.dedup(true, Some(1.0)).await.merges.is_empty(), "0.995 is below a 1.0 threshold");
        assert_eq!(f.dedup(true, Some(0.99)).await.merges.len(), 1);
    }

    #[tokio::test]
    async fn decisions_without_embeddings_match_only_by_exact_text_and_are_reported() {
        let f = fixture().await;
        f.decision("Adopt an event-sourced audit log.", T0).await;
        f.decision("We will keep audit history as a stream of events.", T1).await;

        let result = f.dedup(true, None).await;
        assert!(result.merges.is_empty());
        assert_eq!(result.decisions_without_embedding, 2);
        assert!(result.warnings.iter().any(|w| w.contains("embed_all")));
    }

    /// An ADR decision is evidence, never merged away: an older duplicate episode
    /// decision folds into it even though the ADR was ingested later.
    #[tokio::test]
    async fn adr_decision_is_only_ever_a_survivor() {
        use crate::tools::sync_adrs::{self, SyncAdrsFromGitParams};
        let f = fixture().await;
        let (ep, _) = f.decision("Use the shared bus for events.", "2020-01-01T00:00:00Z").await;

        std::fs::create_dir_all(format!("{}/docs/adr", f.repo_path)).unwrap();
        git2::Repository::init(&f.repo_path).unwrap();
        std::fs::write(
            format!("{}/docs/adr/0001-bus.md", f.repo_path),
            "# ADR-0001: Bus\n\n## Status\nAccepted\n\n## Context\nc\n\n## Decision\nUse the shared bus for events.\n",
        )
        .unwrap();
        sync_adrs::run(
            &f.store,
            SyncAdrsFromGitParams { repo_path: f.repo_path.clone(), adr_glob: "docs/adr/*.md".into() },
        )
        .await
        .unwrap();

        let result = f.dedup(false, None).await;
        assert_eq!(result.merges.len(), 1, "{:#?}", result.merges);
        assert_eq!(result.merges[0].loser_id, ep.to_string());
        assert!(!f.is_open(ep).await);
        let survivor = Uuid::parse_str(&result.merges[0].survivor_id).unwrap();
        assert_ne!(survivor, ep);
        assert!(f.is_open(survivor).await, "the ADR decision survives");
    }
}
