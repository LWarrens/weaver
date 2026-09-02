//! Cross-ADR consistency checking and conflict explanation.
//!
//! Three detectors, all read-only and never auto-resolving:
//!
//! 1. **Explicit edges** — open `conflicts_with` temporal edges between
//!    decisions (emitted by episode fact extraction or future tools).
//! 2. **Contradictory constraints** — pairs of open constraints from
//!    different decisions with opposite obligation polarity (one negated,
//!    one not) over substantially shared terms; confidence rises when the
//!    two decisions also govern overlapping files.
//! 3. **Supersession inconsistencies** — superseded ADRs still open, and
//!    mutual (cyclic) supersession.
//!
//! Results land in `ArchResponse.conflicts` as explained candidates with
//! per-conflict confidence, satisfying NFR-002: inferred conflicts are
//! never indistinguishable from accepted architectural truth.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::domain::entities::{ArchResponse, Polarity, TemporalContext};
use crate::error::Error;
use crate::storage::SqliteStore;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckConsistencyParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Optional ISO-8601 timestamp to evaluate as-of. Defaults to now.
    #[serde(default)]
    pub valid_at: Option<String>,
    /// Minimum confidence (0.0–1.0) for reported conflicts. Default 0.0.
    #[serde(default)]
    pub min_confidence: f64,
}

// ---------------------------------------------------------------------------
// Heuristics
// ---------------------------------------------------------------------------

/// Pairwise constraint comparison is O(n²); above this we skip it with a warning.
const MAX_PAIRWISE_CONSTRAINTS: usize = 400;

const NEGATION_MARKERS: &[&str] = &[
    "must not",
    "shall not",
    "should not",
    "may not",
    "do not",
    "don't",
    "never",
    "prohibit",
    "forbid",
    "avoid",
];

/// Obligation and glue words excluded from term overlap so that shared
/// vocabulary reflects subject matter, not sentence shape.
const STOPWORDS: &[&str] = &[
    "must", "shall", "should", "will", "with", "that", "this", "from", "into", "have", "when",
    "then", "than", "them", "they", "each", "every", "always", "never", "only", "over", "under",
    "through", "directly", "avoid", "prohibited", "forbidden", "their", "there", "these", "those",
    "been", "being", "would", "could", "about",
];

fn has_negation(text: &str) -> bool {
    let t = text.to_lowercase();
    NEGATION_MARKERS.iter().any(|m| t.contains(m))
}

fn content_terms(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: CheckConsistencyParams,
) -> Result<ArchResponse, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path).map_err(|_| Error::InvalidInput {
        field: "repo_path",
        reason: format!("path does not exist: {}", params.repo_path),
    })?;
    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;
    let repo = store.upsert_repository(repo_path_str, None).await?;

    let now = Utc::now().to_rfc3339();
    let valid_at = params.valid_at.as_deref();
    let min_confidence = params.min_confidence.clamp(0.0, 1.0);

    let decisions = store.list_all_decisions(repo.id, valid_at).await?;
    let decision_ids: Vec<String> = decisions.iter().map(|d| d.id.clone()).collect();
    let meta: HashMap<&str, (&str, &str, &str)> = decisions
        .iter()
        .map(|d| {
            (
                d.id.as_str(),
                (d.adr_id.as_str(), d.title.as_str(), d.status.as_str()),
            )
        })
        .collect();
    let label = |decision_id: &str| -> String {
        match meta.get(decision_id) {
            Some((adr_id, title, _)) => format!("{} '{}'", adr_id, title),
            None => format!("decision {}", decision_id),
        }
    };

    let mut warnings: Vec<String> = Vec::new();
    let mut conflicts: Vec<serde_json::Value> = Vec::new();

    // --- 1. Explicit conflicts_with edges ---------------------------------
    let mut explicit = 0usize;
    for edge in store
        .open_conflict_edges_for_decisions(&decision_ids)
        .await?
    {
        conflicts.push(json!({
            "kind": "explicit_edge",
            "confidence": edge.confidence,
            "source_decision_id": edge.source_id,
            "target_decision_id": edge.target_id,
            "evidence_refs": edge.evidence_refs.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            "explanation": format!(
                "{} conflicts with {} (explicit conflicts_with edge, recorded {})",
                label(&edge.source_id), label(&edge.target_id), edge.ingested_at
            ),
        }));
        explicit += 1;
    }

    // --- 2. Contradictory constraints -------------------------------------
    let constraints = store.find_constraints_for_decisions(&decision_ids).await?;
    let mut contradictory = 0usize;
    if constraints.len() > MAX_PAIRWISE_CONSTRAINTS {
        warnings.push(format!(
            "constraint set too large for pairwise contradiction check ({} > {}); skipped",
            constraints.len(),
            MAX_PAIRWISE_CONSTRAINTS
        ));
    } else {
        // Prefer the polarity recorded on the constraint's claim at ingest
        // (docs/DESIGN-claims-and-freshness.md); fall back to a text scan when
        // no claim exists yet.
        let constraint_subject_ids: Vec<Uuid> = constraints
            .iter()
            .filter_map(|c| Uuid::parse_str(&c.id).ok())
            .collect();
        let claim_polarity: HashMap<String, Polarity> = store
            .claims_for_subjects(&constraint_subject_ids)
            .await?
            .into_iter()
            .filter(|cl| cl.subject_type == "constraint")
            .filter_map(|cl| cl.polarity.map(|p| (cl.subject_id.to_string(), p)))
            .collect();

        // Precompute polarity, terms, and each decision's linked files.
        let analyzed: Vec<(bool, HashSet<String>)> = constraints
            .iter()
            .map(|c| {
                let negated = match claim_polarity.get(&c.id) {
                    Some(Polarity::MustNot) => true,
                    Some(Polarity::Must) => false,
                    None => has_negation(&c.text),
                };
                (negated, content_terms(&c.text))
            })
            .collect();
        let mut files: HashMap<String, HashSet<String>> = HashMap::new();
        for c in &constraints {
            if !files.contains_key(&c.decision_id) {
                let paths = store
                    .file_paths_linked_to_decision(&c.decision_id)
                    .await?
                    .into_iter()
                    .collect();
                files.insert(c.decision_id.clone(), paths);
            }
        }

        for i in 0..constraints.len() {
            for j in (i + 1)..constraints.len() {
                let (a, b) = (&constraints[i], &constraints[j]);
                if a.decision_id == b.decision_id {
                    continue;
                }
                let (neg_a, terms_a) = &analyzed[i];
                let (neg_b, terms_b) = &analyzed[j];
                if neg_a == neg_b {
                    continue;
                }
                let shared: Vec<&String> = terms_a.intersection(terms_b).collect();
                let smaller = terms_a.len().min(terms_b.len());
                if shared.len() < 2 || smaller == 0 || shared.len() * 2 < smaller {
                    continue;
                }

                let empty = HashSet::new();
                let fa = files.get(&a.decision_id).unwrap_or(&empty);
                let fb = files.get(&b.decision_id).unwrap_or(&empty);
                let shared_files: Vec<&String> = fa.intersection(fb).collect();
                let confidence = if shared_files.is_empty() { 0.5 } else { 0.75 };

                let mut shared_terms: Vec<&str> = shared.iter().map(|s| s.as_str()).collect();
                shared_terms.sort_unstable();
                let mut shared_file_list: Vec<&str> =
                    shared_files.iter().map(|s| s.as_str()).collect();
                shared_file_list.sort_unstable();

                conflicts.push(json!({
                    "kind": "contradictory_constraints",
                    "confidence": confidence,
                    "constraint_ids": [a.id, b.id],
                    "decision_ids": [a.decision_id, b.decision_id],
                    "shared_terms": shared_terms,
                    "shared_files": shared_file_list,
                    "explanation": format!(
                        "Constraint '{}' ({}) and constraint '{}' ({}) make opposite-polarity claims over shared terms [{}]{}",
                        a.text.trim(), label(&a.decision_id),
                        b.text.trim(), label(&b.decision_id),
                        shared_terms.join(", "),
                        if shared_file_list.is_empty() {
                            String::new()
                        } else {
                            format!("; both govern [{}]", shared_file_list.join(", "))
                        }
                    ),
                }));
                contradictory += 1;
            }
        }
    }

    // --- 3. Supersession inconsistencies ----------------------------------
    let pairs = store.supersession_pairs_with_status(repo.id).await?;
    let pair_set: HashSet<(&str, &str)> = pairs
        .iter()
        .map(|p| (p.0.as_str(), p.1.as_str()))
        .collect();
    let mut supersession = 0usize;
    for (superseder, superseded, status, valid_to) in &pairs {
        if pair_set.contains(&(superseded.as_str(), superseder.as_str()))
            && superseder.as_str() < superseded.as_str()
        {
            conflicts.push(json!({
                "kind": "supersession_cycle",
                "confidence": 0.9,
                "adr_ids": [superseder, superseded],
                "explanation": format!(
                    "{} and {} each claim to supersede the other — supersession cycle",
                    superseder, superseded
                ),
            }));
            supersession += 1;
        }
        if valid_to.is_none() {
            conflicts.push(json!({
                "kind": "superseded_but_active",
                "confidence": 0.8,
                "adr_ids": [superseder, superseded],
                "explanation": format!(
                    "{} is superseded by {} but is still open (status '{}'); re-run sync_adrs_from_git or retract it",
                    superseded, superseder, status
                ),
            }));
            supersession += 1;
        }
    }

    conflicts.retain(|c| c["confidence"].as_f64().unwrap_or(0.0) >= min_confidence);
    conflicts.sort_by(|a, b| {
        b["confidence"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["confidence"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut resp = ArchResponse::empty();
    resp.answer = Some(if conflicts.is_empty() {
        format!(
            "No conflicts detected across {} decision(s) and {} constraint(s).",
            decisions.len(),
            constraints.len()
        )
    } else {
        format!(
            "{} potential conflict(s): {} explicit edge(s), {} contradictory constraint pair(s), {} supersession issue(s).",
            conflicts.len(),
            explicit,
            contradictory,
            supersession
        )
    });
    resp.conflicts = conflicts;
    resp.warnings = warnings;
    resp.temporal_context = TemporalContext {
        valid_at: params.valid_at.clone(),
        ingested_at: Some(now),
    };
    resp.confidence = 1.0;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_adrs::{self, SyncAdrsFromGitParams};

    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    async fn sync_fixture_adrs(store: &Arc<SqliteStore>, dir: &std::path::Path) {
        sync_adrs::run(
            store,
            SyncAdrsFromGitParams {
                repo_path: dir.to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync adrs");
    }

    #[test]
    fn negation_and_terms() {
        assert!(has_negation("Services must not use the queue."));
        assert!(!has_negation("Services must use the queue."));
        let terms = content_terms("Services must use the shared event queue.");
        assert!(terms.contains("services") && terms.contains("queue"));
        assert!(!terms.contains("must"));
    }

    #[tokio::test]
    async fn detects_contradictory_constraints_across_adrs() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(
            dir.path().join("0001-use-queue.md"),
            r#"# ADR-0001: Use the event queue

## Status

Accepted

## Decision

All services must use the shared event queue for messaging in `src/bus.rs`.
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("0002-no-queue.md"),
            r#"# ADR-0002: Direct calls only

## Status

Accepted

## Decision

Services must not use the shared event queue in `src/bus.rs`.
"#,
        )
        .unwrap();
        sync_fixture_adrs(&store, dir.path()).await;

        let resp = run(
            &store,
            CheckConsistencyParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                valid_at: None,
                min_confidence: 0.0,
            },
        )
        .await
        .expect("check");

        let contradictions: Vec<_> = resp
            .conflicts
            .iter()
            .filter(|c| c["kind"] == "contradictory_constraints")
            .collect();
        assert_eq!(
            contradictions.len(),
            1,
            "conflicts: {:#?}",
            resp.conflicts
        );
        let c = contradictions[0];
        assert_eq!(c["confidence"], 0.75, "shared file boosts confidence: {c:#?}");
        assert!(
            c["shared_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "src/bus.rs"),
            "{c:#?}"
        );
        let explanation = c["explanation"].as_str().unwrap();
        assert!(explanation.contains("ADR-0001") && explanation.contains("ADR-0002"));

        // min_confidence filters it out
        let filtered = run(
            &store,
            CheckConsistencyParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                valid_at: None,
                min_confidence: 0.9,
            },
        )
        .await
        .expect("check filtered");
        assert!(filtered
            .conflicts
            .iter()
            .all(|c| c["kind"] != "contradictory_constraints"));
    }

    #[tokio::test]
    async fn reports_explicit_conflict_edges() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(
            dir.path().join("0001-a.md"),
            "# ADR-0001: Alpha\n\n## Status\n\nAccepted\n\n## Decision\n\nWe choose approach alpha for the ingest pipeline.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("0002-b.md"),
            "# ADR-0002: Beta\n\n## Status\n\nAccepted\n\n## Decision\n\nWe choose approach beta for the export pipeline.\n",
        )
        .unwrap();
        sync_fixture_adrs(&store, dir.path()).await;

        let repo = store
            .upsert_repository(dir.path().to_str().unwrap(), None)
            .await
            .expect("repo");
        let decisions = store.list_all_decisions(repo.id, None).await.expect("all");
        assert_eq!(decisions.len(), 2);
        store
            .insert_test_temporal_edge(
                "conflicts_with",
                &decisions[0].id,
                &decisions[1].id,
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .expect("edge");

        let resp = run(
            &store,
            CheckConsistencyParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                valid_at: None,
                min_confidence: 0.0,
            },
        )
        .await
        .expect("check");

        let explicit: Vec<_> = resp
            .conflicts
            .iter()
            .filter(|c| c["kind"] == "explicit_edge")
            .collect();
        assert_eq!(explicit.len(), 1, "conflicts: {:#?}", resp.conflicts);
        let explanation = explicit[0]["explanation"].as_str().unwrap();
        assert!(
            explanation.contains("ADR-0001") && explanation.contains("ADR-0002"),
            "{explanation}"
        );
        assert!(resp.answer.as_deref().unwrap().contains("1 explicit edge"));
    }

    #[tokio::test]
    async fn clean_repo_reports_no_conflicts() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("0001-a.md"),
            "# ADR-0001: Alpha\n\n## Status\n\nAccepted\n\n## Decision\n\nWe choose approach alpha.\n",
        )
        .unwrap();
        sync_fixture_adrs(&store, dir.path()).await;

        let resp = run(
            &store,
            CheckConsistencyParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                valid_at: None,
                min_confidence: 0.0,
            },
        )
        .await
        .expect("check");
        assert!(resp.conflicts.is_empty(), "{:#?}", resp.conflicts);
        assert!(resp.answer.as_deref().unwrap().contains("No conflicts"));
    }
}
