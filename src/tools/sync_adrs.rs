use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use glob::{glob, Pattern};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::adr_parser;
use crate::domain::adr_parser::ParsedAdr;
use crate::domain::entities::{
    AdrDocument, AdrStatus, AnchorIdentity, AnchorSource, Constraint, Decision, DecisionCodeLink,
    EvidenceGrade, LinkSource, LinkType, Locator,
};
use crate::domain::{anchors, claims};
use crate::embeddings::{pack_f32, provider_from_env};
use crate::error::Error;
use crate::storage::sqlite::TemporalEdge;
use crate::storage::SqliteStore;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncAdrsFromGitParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// Glob pattern for ADR files relative to the repository root.
    /// Example: `"docs/adr/*.md"` or `"**/*.md"`.
    pub adr_glob: String,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

const MAX_WARNINGS: usize = 20;

#[derive(Debug, Serialize)]
pub struct SyncAdrsResult {
    pub synced: usize,
    pub unchanged: usize,
    pub closed: usize,
    /// First `MAX_WARNINGS` warning strings. Check `warnings_total` for the full count.
    pub warnings: Vec<String>,
    /// Total number of warnings generated (including those suppressed from the list).
    pub warnings_total: usize,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    store: &Arc<SqliteStore>,
    params: SyncAdrsFromGitParams,
) -> Result<SyncAdrsResult, Error> {
    // --- Input validation ---------------------------------------------------
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!(
                "path does not exist or is not accessible: {}",
                params.repo_path
            ),
        })?;

    if params.adr_glob.is_empty() {
        return Err(Error::InvalidInput {
            field: "adr_glob",
            reason: "glob pattern must not be empty".to_string(),
        });
    }

    // Reject glob patterns containing path traversal sequences
    if params.adr_glob.contains("..") {
        return Err(Error::InvalidInput {
            field: "adr_glob",
            reason: "glob pattern must not contain '..'".to_string(),
        });
    }

    // --- Resolve repository -------------------------------------------------
    let repo_name = repo_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);

    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let repo = store
        .upsert_repository(repo_path_str, repo_name.as_deref())
        .await?;

    // --- Collect matching files ---------------------------------------------
    let pattern = repo_path.join(&params.adr_glob);
    let pattern_str = pattern.to_str().ok_or_else(|| Error::InvalidInput {
        field: "adr_glob",
        reason: "resulting path contains non-UTF-8 characters".to_string(),
    })?;

    // Validate the glob pattern before executing
    let mut paths: Vec<PathBuf> = glob(pattern_str)
        .map_err(|e| Error::InvalidInput {
            field: "adr_glob",
            reason: format!("invalid glob pattern: {}", e),
        })?
        .filter_map(|entry| match entry {
            Ok(p) => Some(p),
            Err(_) => None,
        })
        .filter(|p| p.is_file())
        .collect();

    // Filter out gitignored paths and any patterns listed in .archignore.
    // This prevents broad globs like **/*.md from sweeping node_modules/ etc.
    let git_repo = git2::Repository::open(&repo_path).ok();
    let archignore = load_archignore(&repo_path);
    paths.retain(|p| {
        let rel = match p.strip_prefix(&repo_path) {
            Ok(r) => r,
            Err(_) => return true, // security check below will reject these
        };
        if let Some(ref repo) = git_repo {
            if repo.is_path_ignored(rel).unwrap_or(false) {
                return false;
            }
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        !archignore.iter().any(|pat| pat.matches(&rel_str))
    });

    let now = Utc::now().to_rfc3339();
    let embedding_provider = provider_from_env();
    let mut synced = 0usize;
    let mut unchanged = 0usize;
    let mut warnings: Vec<String> = vec![];

    for path in &paths {
        // Security: verify the resolved file is inside the repository
        let canonical = dunce::canonicalize(path).map_err(|e| Error::InvalidInput {
            field: "adr_glob",
            reason: format!("cannot canonicalize matched path: {}", e),
        })?;

        if !canonical.starts_with(&repo_path) {
            warnings.push(format!(
                "skipped: {} is outside repository root",
                path.display()
            ));
            continue;
        }

        // Relative path for storage (used as source_uri)
        let rel = canonical
            .strip_prefix(&repo_path)
            .expect("just checked starts_with")
            .to_string_lossy()
            .replace('\\', "/"); // normalise to forward slashes

        let source = match std::fs::read_to_string(&canonical) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!("skipped {}: {}", rel, e));
                continue;
            }
        };

        let stem = canonical
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let parsed = adr_parser::parse(&source, stem);

        // Skip files that carry no detectable ADR identity — they are not ADR documents.
        let adr_id_str = match parsed.adr_id.clone() {
            Some(id) => id,
            None => continue,
        };

        // Check if the parsed query-facing fields are unchanged.
        if let Some(existing) = store.find_current_adr(repo.id, &rel).await? {
            let existing_constraints = store.find_constraint_facts_for_adr(existing.id).await?;

            if parsed_adr_matches_existing(&existing, &parsed, &adr_id_str, &existing_constraints) {
                unchanged += 1;
                continue;
            }

            // Content changed: close the old record
            store.close_adr(existing.id, &now).await?;
            // Also close related decisions/constraints
            store.close_decisions_for_adr(existing.id, &now).await?;
        }

        let effective_from = adr_effective_from(&parsed, &repo_path, &rel, &now, &mut warnings);

        // Insert the new ADR document
        let doc_id = Uuid::new_v4();
        let status = parsed.status.clone().unwrap_or(AdrStatus::Unknown);

        let doc = AdrDocument {
            id: doc_id,
            repo_id: repo.id,
            adr_id: adr_id_str.clone(),
            title: parsed.title.clone(),
            status,
            date: parsed.date.clone(),
            context: parsed.context.clone(),
            decision: parsed.decision.clone(),
            consequences: parsed.consequences.clone(),
            supersedes: parsed.supersedes.clone(),
            superseded_by: parsed.superseded_by.clone(),
            file_mentions: parsed.file_mentions.clone(),
            service_mentions: parsed.service_mentions.clone(),
            module_mentions: parsed.module_mentions.clone(),
            source_uri: rel.clone(),
            effective_from: Some(effective_from.clone()),
            effective_to: None,
            valid_from: now.clone(),
            valid_to: None,
            ingested_at: now.clone(),
            source_time: parsed.date.clone(),
            confidence: 1.0,
        };

        store.insert_adr(&doc).await?;

        // Insert one Decision record from the decision section text
        if let Some(decision_text) = &parsed.decision {
            let decision = Decision {
                id: Uuid::new_v4(),
                title: None,
                adr_id: Some(doc_id),
                episode_id: None,
                text: decision_text.clone(),
                source_uri: rel.clone(),
                valid_from: now.clone(),
                valid_to: None,
                ingested_at: now.clone(),
                source_time: Some(effective_from.clone()),
                confidence: 1.0,
                evidence_refs: vec![],
            };
            let decision_id = decision.id;
            store.insert_decision(&decision).await?;

            // Claim + anchor: the decision assertion, cited to the ADR "Decision"
            // section. Deterministic parse → evidence grade `proven`. read_set is
            // the ADR section plus every file the ADR mentions, so completeness
            // is checkable.
            let mut read_set = vec![AnchorIdentity {
                source_kind: AnchorSource::Adr,
                source_uri: rel.clone(),
                subpath: "Decision".to_string(),
            }];
            read_set.extend(parsed.file_mentions.iter().map(|f| AnchorIdentity {
                source_kind: AnchorSource::SourceFile,
                source_uri: f.clone(),
                subpath: String::new(),
            }));
            let decision_claim = claims::decision_claim(
                repo.id,
                decision_id,
                decision_text,
                EvidenceGrade::Proven,
                read_set,
                &now,
                Some(effective_from.clone()),
                1.0,
            );
            store.insert_claim(&decision_claim).await?;
            store
                .insert_evidence_anchor(&anchors::build_anchor(
                    repo.id,
                    decision_claim.id,
                    AnchorIdentity {
                        source_kind: AnchorSource::Adr,
                        source_uri: rel.clone(),
                        subpath: "Decision".to_string(),
                    },
                    Locator::Section {
                        name: "Decision".to_string(),
                    },
                    decision_text.clone(),
                    None,
                    None,
                    &now,
                    Some(effective_from.clone()),
                ))
                .await?;

            // defines edge: AdrDocument → Decision
            let defines_edge = TemporalEdge {
                id: Uuid::new_v4(),
                edge_type: "defines".to_string(),
                source_id: doc_id.to_string(),
                source_type: "adr_document".to_string(),
                target_id: decision_id.to_string(),
                target_type: "decision".to_string(),
                valid_from: now.clone(),
                valid_to: None,
                ingested_at: now.clone(),
                confidence: 1.0,
                evidence_refs: vec![],
            };
            store.insert_temporal_edge(&defines_edge).await?;

            // Embed the decision text if a provider is configured
            if let Some(provider) = embedding_provider.as_ref() {
                if let Ok(vec) = provider.embed(decision_text).await {
                    if !vec.is_empty() {
                        let blob = pack_f32(&vec);
                        let _ = store.update_decision_embedding(decision_id, &blob).await;
                    }
                }
            }

            // Insert extracted constraints
            for ec in &parsed.constraints {
                let constraint = Constraint {
                    id: Uuid::new_v4(),
                    decision_id,
                    text: ec.text.clone(),
                    source_uri: rel.clone(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    source_time: Some(effective_from.clone()),
                    confidence: ec.confidence,
                    evidence_refs: vec![],
                };
                let constraint_id = constraint.id;
                store.insert_constraint(&constraint).await?;

                // Claim + anchor: the obligation sentence, cited to the ADR
                // section it was extracted from. Polarity auto-detected.
                let section = if ec.section.is_empty() {
                    "Decision".to_string()
                } else {
                    ec.section.clone()
                };
                let constraint_claim = claims::constraint_claim(
                    repo.id,
                    constraint_id,
                    &ec.text,
                    EvidenceGrade::Proven,
                    vec![AnchorIdentity {
                        source_kind: AnchorSource::Adr,
                        source_uri: rel.clone(),
                        subpath: section.clone(),
                    }],
                    &now,
                    Some(effective_from.clone()),
                    ec.confidence,
                );
                store.insert_claim(&constraint_claim).await?;
                store
                    .insert_evidence_anchor(&anchors::build_anchor(
                        repo.id,
                        constraint_claim.id,
                        AnchorIdentity {
                            source_kind: AnchorSource::Adr,
                            source_uri: rel.clone(),
                            subpath: section.clone(),
                        },
                        Locator::Section { name: section },
                        ec.text.clone(),
                        None,
                        None,
                        &now,
                        Some(effective_from.clone()),
                    ))
                    .await?;

                // imposes edge: Decision → Constraint
                let imposes_edge = TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "imposes".to_string(),
                    source_id: decision_id.to_string(),
                    source_type: "decision".to_string(),
                    target_id: constraint_id.to_string(),
                    target_type: "constraint".to_string(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    confidence: 1.0,
                    evidence_refs: vec![],
                };
                store.insert_temporal_edge(&imposes_edge).await?;

                // Embed the constraint text if a provider is configured
                if let Some(provider) = embedding_provider.as_ref() {
                    if let Ok(vec) = provider.embed(&ec.text).await {
                        if !vec.is_empty() {
                            let blob = pack_f32(&vec);
                            let _ = store.update_constraint_embedding(constraint_id, &blob).await;
                        }
                    }
                }
            }

            // Insert decision ↔ file links from ADR text mentions
            for file_path in &parsed.file_mentions {
                let link = DecisionCodeLink {
                    id: Uuid::new_v4(),
                    decision_id,
                    file_path: file_path.clone(),
                    symbol: None,
                    link_type: LinkType::Mentions,
                    link_source: LinkSource::AdrText,
                    confidence: 0.9,
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    evidence_refs: vec![],
                };
                store.insert_decision_code_link(&link).await?;
            }

            // Resolve and link entities (services, modules) mentioned in this ADR
            for (entity_name, entity_type) in parsed
                .service_mentions
                .iter()
                .map(|n| (n, "service"))
                .chain(parsed.module_mentions.iter().map(|n| (n, "module")))
            {
                let entity = store
                    .upsert_entity_node(repo.id, entity_name, Some(entity_type), 0.8, &now)
                    .await?;
                let edge = TemporalEdge {
                    id: Uuid::new_v4(),
                    edge_type: "mentions".to_string(),
                    source_id: decision_id.to_string(),
                    source_type: "decision".to_string(),
                    target_id: entity.id.to_string(),
                    target_type: "entity".to_string(),
                    valid_from: now.clone(),
                    valid_to: None,
                    ingested_at: now.clone(),
                    confidence: 0.85,
                    evidence_refs: vec![doc_id],
                };
                store.insert_temporal_edge(&edge).await?;
            }
        }

        synced += 1;
    }

    // Detect supersession edges across all current ADRs in this repo
    let closed = resolve_supersession_edges(store, repo.id, &now).await?;

    crate::tools::freshness::record_lane(store, repo.id, &repo_path, "adr", "ok").await;

    let warnings_total = warnings.len();
    warnings.truncate(MAX_WARNINGS);
    Ok(SyncAdrsResult {
        synced,
        unchanged,
        closed,
        warnings,
        warnings_total,
    })
}

fn parsed_adr_matches_existing(
    existing: &AdrDocument,
    parsed: &ParsedAdr,
    parsed_adr_id: &str,
    existing_constraints: &[(String, f64)],
) -> bool {
    let parsed_status = parsed.status.clone().unwrap_or(AdrStatus::Unknown);
    let mut parsed_constraints = parsed
        .constraints
        .iter()
        .map(|constraint| (constraint.text.clone(), constraint.confidence))
        .collect::<Vec<_>>();
    parsed_constraints.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

    existing.adr_id == parsed_adr_id
        && existing.title == parsed.title
        && existing.status == parsed_status
        && existing.date == parsed.date
        && existing.context == parsed.context
        && existing.decision == parsed.decision
        && existing.consequences == parsed.consequences
        && existing.supersedes == parsed.supersedes
        && existing.superseded_by == parsed.superseded_by
        && existing.file_mentions == parsed.file_mentions
        && existing.service_mentions == parsed.service_mentions
        && existing.module_mentions == parsed.module_mentions
        && existing_constraints == parsed_constraints.as_slice()
}

// ---------------------------------------------------------------------------
// Supersession resolution
// ---------------------------------------------------------------------------

/// For every current ADR that declares supersedes/superseded_by, create the
/// corresponding edge and close the superseded ADR's valid_to.
async fn resolve_supersession_edges(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    now: &str,
) -> Result<usize, Error> {
    let adrs = store.list_current_adrs(repo_id).await?;
    let mut closed = 0usize;

    for adr in &adrs {
        for superseded_adr_id in &adr.supersedes {
            if let Some(old) = store.find_adr_by_adr_id(repo_id, superseded_adr_id).await? {
                store
                    .upsert_supersession_edge(
                        adr.id,
                        old.id,
                        adr.effective_from.as_deref().unwrap_or(&adr.valid_from),
                        now,
                    )
                    .await?;

                // Close the superseded ADR if it doesn't already have a valid_to
                if old.valid_to.is_none() {
                    store.close_adr(old.id, now).await?;
                    closed += 1;
                }
            }
        }
    }

    Ok(closed)
}

/// Read `.archignore` from the repository root and compile each non-comment line
/// into a glob Pattern. Callers use these to exclude paths from ADR ingestion.
fn load_archignore(repo_path: &std::path::Path) -> Vec<Pattern> {
    std::fs::read_to_string(repo_path.join(".archignore"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|pat| Pattern::new(pat).ok())
        .collect()
}

fn adr_effective_from(
    parsed: &ParsedAdr,
    repo_path: &std::path::Path,
    rel: &str,
    now: &str,
    warnings: &mut Vec<String>,
) -> String {
    if let Some(date) = parsed.date.as_deref().and_then(normalize_date) {
        return date;
    }

    if let Some(date) = git_author_date(repo_path, rel) {
        return date;
    }

    warnings.push(format!(
        "no Date header or git author date for {}; using ingestion time as effective_from",
        rel
    ));
    now.to_string()
}

fn normalize_date(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if chrono::DateTime::parse_from_rfc3339(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().to_rfc3339())
}

fn git_author_date(repo_path: &std::path::Path, rel: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("log")
        .arg("--format=%aI")
        .arg("--")
        .arg(rel)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .last()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::TemporalMode;
    use crate::tools::find_decisions::{FindDecisionsForCodeParams, Target};

    /// Spin up an in-memory SQLite store with migrations applied.
    async fn test_store() -> Arc<SqliteStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        Arc::new(store)
    }

    /// A minimal Nygard-format ADR that mentions a specific source file.
    const FIXTURE_ADR: &str = r#"# ADR-0001 Use SQLite for local storage

## Status

Accepted. Date: 2024-01-15.

## Context

We need a lightweight, zero-dependency database for local operation.

## Decision

We will use SQLite via sqlx. The module `src/storage/db.rs` must implement
the repository interface. All queries must use parameterized statements.

## Consequences

- Queries must never use string interpolation.
- The file `src/storage/db.rs` owns all SQL.
"#;

    #[tokio::test]
    async fn sync_creates_adr_and_decision() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(dir.path().join("0001-use-sqlite.md"), FIXTURE_ADR).expect("write fixture");

        let result = run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        assert_eq!(result.synced, 1, "one ADR should be synced");
        assert_eq!(result.unchanged, 0);
        assert!(
            result.warnings.is_empty(),
            "unexpected warnings: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn sync_populates_claims_and_anchors() {
        use crate::domain::entities::{EvidenceGrade, Polarity};

        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("0001-use-sqlite.md"), FIXTURE_ADR).expect("write fixture");

        run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let repo_path = dir.path().canonicalize().expect("canonical");
        let repo = store
            .upsert_repository(repo_path.to_str().unwrap(), None)
            .await
            .expect("repo");
        let decisions = store
            .list_all_decisions(repo.id, None)
            .await
            .expect("decisions");
        assert_eq!(decisions.len(), 1);
        let decision_id = Uuid::parse_str(&decisions[0].id).unwrap();

        let dclaims = store
            .claims_for_subject("decision", decision_id)
            .await
            .expect("decision claims");
        assert_eq!(dclaims.len(), 1, "one decision claim");
        assert_eq!(dclaims[0].evidence_grade, EvidenceGrade::Proven);

        let danchors = store
            .anchors_for_claim(dclaims[0].id)
            .await
            .expect("decision anchors");
        assert_eq!(danchors.len(), 1);
        assert_eq!(danchors[0].identity.subpath, "Decision");
        assert!(!danchors[0].content_hash.is_empty());

        // Constraint claims carry detected polarity ("must never" / "must not").
        let all_claims = store
            .claims_for_subjects(&[decision_id])
            .await
            .expect("claims for subjects");
        // subject is the constraint id, not the decision id — fetch via the
        // constraint rows instead.
        let constraints = store
            .find_constraints_for_decisions(&[decisions[0].id.clone()])
            .await
            .expect("constraints");
        assert!(!constraints.is_empty());
        let cid = Uuid::parse_str(&constraints[0].id).unwrap();
        let cclaims = store
            .claims_for_subject("constraint", cid)
            .await
            .expect("constraint claims");
        assert_eq!(cclaims.len(), 1);
        assert!(matches!(
            cclaims[0].polarity,
            Some(Polarity::Must) | Some(Polarity::MustNot)
        ));
        let _ = all_claims;
    }

    #[tokio::test]
    async fn sync_is_idempotent() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("0001-use-sqlite.md"), FIXTURE_ADR).expect("write fixture");

        let params = || SyncAdrsFromGitParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            adr_glob: "*.md".to_string(),
        };

        run(&store, params()).await.expect("first sync");
        let second = run(&store, params()).await.expect("second sync");

        assert_eq!(
            second.synced, 0,
            "re-sync of unchanged ADR should not increment synced"
        );
        assert_eq!(second.unchanged, 1, "unchanged should be 1 on re-sync");
    }

    #[tokio::test]
    async fn changed_adr_preserves_closed_decision_history() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr_path = dir.path().join("0001-use-sqlite.md");
        std::fs::write(&adr_path, FIXTURE_ADR).expect("write fixture");

        let params = || SyncAdrsFromGitParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            adr_glob: "*.md".to_string(),
        };

        run(&store, params()).await.expect("first sync");

        let repo_path = dir.path().canonicalize().expect("canonical repo path");
        let repo = store
            .upsert_repository(
                repo_path.to_str().unwrap(),
                repo_path.file_name().and_then(|n| n.to_str()),
            )
            .await
            .expect("repository");
        let initial_decisions = store
            .list_all_decisions(repo.id, None)
            .await
            .expect("initial decisions");
        assert_eq!(initial_decisions.len(), 1);
        let initial_decision = initial_decisions[0].clone();
        let initial_constraints = store
            .find_constraints_for_decisions(std::slice::from_ref(&initial_decision.id))
            .await
            .expect("initial constraints");
        assert!(
            !initial_constraints.is_empty(),
            "fixture should extract at least one constraint"
        );

        std::fs::write(
            &adr_path,
            FIXTURE_ADR.replace(
                "We will use SQLite via sqlx.",
                "We will use SQLite via sqlx with WAL enabled.",
            ),
        )
        .expect("write changed fixture");

        run(&store, params()).await.expect("second sync");

        let current_decisions = store
            .list_all_decisions(repo.id, None)
            .await
            .expect("current decisions");
        assert_eq!(current_decisions.len(), 1);
        assert_ne!(current_decisions[0].id, initial_decision.id);

        let historical_decisions = store
            .list_all_decisions(repo.id, Some(&initial_decision.valid_from))
            .await
            .expect("historical decisions");
        assert!(
            historical_decisions
                .iter()
                .any(|d| d.id == initial_decision.id),
            "old decision should remain queryable at its original valid time"
        );

        let historical_constraints = store
            .find_constraints_for_decisions(std::slice::from_ref(&initial_decision.id))
            .await
            .expect("historical constraints");
        assert_eq!(historical_constraints.len(), initial_constraints.len());
    }

    #[tokio::test]
    async fn sync_resyncs_consequences_only_constraint_change() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr_path = dir.path().join("0001-use-sqlite.md");
        std::fs::write(&adr_path, FIXTURE_ADR).expect("write fixture");

        let params = || SyncAdrsFromGitParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            adr_glob: "*.md".to_string(),
        };

        run(&store, params()).await.expect("first sync");

        let updated = FIXTURE_ADR.replace(
            "- Queries must never use string interpolation.",
            "- Queries must always use prepared statements.",
        );
        std::fs::write(&adr_path, updated).expect("write updated fixture");

        let second = run(&store, params()).await.expect("second sync");

        assert_eq!(
            second.synced, 1,
            "consequences-only constraint changes should resync"
        );
        assert_eq!(second.unchanged, 0);

        let repo_path = dir.path().canonicalize().expect("canonical repo path");
        let repo_path_str = repo_path.to_str().expect("utf-8 repo path");
        let repo = store
            .upsert_repository(
                repo_path_str,
                repo_path.file_name().and_then(|n| n.to_str()),
            )
            .await
            .expect("repo");
        let current = store
            .find_current_adr(repo.id, "0001-use-sqlite.md")
            .await
            .expect("current ADR")
            .expect("current ADR exists");
        let constraints = store
            .find_constraint_facts_for_adr(current.id)
            .await
            .expect("constraints");

        assert!(
            constraints
                .iter()
                .any(|(text, _)| text == "Queries must always use prepared statements."),
            "expected updated consequence constraint, got {:?}",
            constraints
        );
        assert!(
            constraints
                .iter()
                .all(|(text, _)| text != "Queries must never use string interpolation."),
            "stale consequence constraint should have been removed: {:?}",
            constraints
        );
    }

    #[tokio::test]
    async fn sync_resyncs_context_only_mention_change() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr_path = dir.path().join("0001-use-sqlite.md");
        std::fs::write(&adr_path, FIXTURE_ADR).expect("write fixture");

        let params = || SyncAdrsFromGitParams {
            repo_path: dir.path().to_str().unwrap().to_string(),
            adr_glob: "*.md".to_string(),
        };

        run(&store, params()).await.expect("first sync");

        let updated = FIXTURE_ADR.replace(
            "We need a lightweight, zero-dependency database for local operation.",
            "We need a lightweight, zero-dependency database for local operation in `src/storage/cache.rs`.",
        );
        std::fs::write(&adr_path, updated).expect("write updated fixture");

        let second = run(&store, params()).await.expect("second sync");

        assert_eq!(
            second.synced, 1,
            "context-only mention changes should resync"
        );
        assert_eq!(second.unchanged, 0);
    }

    #[tokio::test]
    async fn find_decisions_returns_linked_file() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("0001-use-sqlite.md"), FIXTURE_ADR).expect("write fixture");

        run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let response = crate::tools::find_decisions::run(
            &store,
            FindDecisionsForCodeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                target: Target {
                    // The ADR mentions "src/storage/db.rs" in its decision text.
                    file: Some("src/storage/db.rs".to_string()),
                    symbol: None,
                    module: None,
                    service: None,
                },
                valid_at: None,
                temporal_mode: Default::default(),
                include_full_text: false,
                verify: None,
            },
        )
        .await
        .expect("find decisions");

        assert!(
            !response.decisions.is_empty(),
            "expected at least one decision linked to src/storage/db.rs; warnings: {:?}",
            response.warnings
        );
        assert_eq!(response.decisions[0].adr_id, "ADR-0001");
    }

    #[tokio::test]
    async fn find_decisions_can_query_event_time_separately_from_ingestion_time() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let adr = r#"# ADR-0002: Use bitemporal history

## Status

Accepted

Date: 2024-03-01

## Decision

We will use bitemporal history for `src/history.rs`.
"#;
        std::fs::write(dir.path().join("0002-bitemporal.md"), adr).expect("write fixture");

        run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync");

        let event_response = crate::tools::find_decisions::run(
            &store,
            FindDecisionsForCodeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                target: Target {
                    file: Some("src/history.rs".to_string()),
                    symbol: None,
                    module: None,
                    service: None,
                },
                valid_at: Some("2024-06-01T00:00:00Z".to_string()),
                temporal_mode: Default::default(),
                include_full_text: false,
                verify: None,
            },
        )
        .await
        .expect("event query");

        assert_eq!(event_response.decisions.len(), 1);
        assert_eq!(event_response.decisions[0].adr_id, "ADR-0002");

        let ingestion_response = crate::tools::find_decisions::run(
            &store,
            FindDecisionsForCodeParams {
                repo_path: dir.path().to_str().unwrap().to_string(),
                target: Target {
                    file: Some("src/history.rs".to_string()),
                    symbol: None,
                    module: None,
                    service: None,
                },
                valid_at: Some("2024-06-01T00:00:00Z".to_string()),
                temporal_mode: TemporalMode::Ingestion,
                include_full_text: false,
                verify: None,
            },
        )
        .await
        .expect("ingestion query");

        assert!(ingestion_response.decisions.is_empty());
    }

    /// End-to-end: sync ADRs → ingest symbols → find decisions by symbol name.
    ///
    /// The ADR mentions `src/storage/db.rs`. We create that file with a function
    /// `query_all`, ingest symbols, then resolve decisions via the symbol name.
    #[tokio::test]
    async fn find_decisions_by_symbol_end_to_end() {
        let store = test_store().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().to_str().unwrap().to_string();

        // Write the ADR that mentions src/storage/db.rs
        std::fs::write(dir.path().join("0001-use-sqlite.md"), FIXTURE_ADR)
            .expect("write ADR fixture");

        // Create the source file referenced by the ADR with a named symbol
        std::fs::create_dir_all(dir.path().join("src/storage")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/storage/db.rs"),
            "pub fn query_all() -> Vec<String> { vec![] }",
        )
        .expect("write source file");

        // Step 1: sync ADRs
        run(
            &store,
            SyncAdrsFromGitParams {
                repo_path: repo_path.clone(),
                adr_glob: "*.md".to_string(),
            },
        )
        .await
        .expect("sync ADRs");

        // Step 2: ingest symbols
        crate::tools::ingest_symbols::run(
            &store,
            crate::tools::ingest_symbols::IngestSymbolsParams {
                repo_path: repo_path.clone(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await
        .expect("ingest symbols");

        // Step 3: find decisions by symbol name
        let response = crate::tools::find_decisions::run(
            &store,
            FindDecisionsForCodeParams {
                repo_path: repo_path.clone(),
                target: Target {
                    file: None,
                    symbol: Some("query_all".to_string()),
                    module: None,
                    service: None,
                },
                valid_at: None,
                temporal_mode: Default::default(),
                include_full_text: false,
                verify: None,
            },
        )
        .await
        .expect("find decisions by symbol");

        assert!(
            !response.decisions.is_empty(),
            "expected decisions for symbol 'query_all'; warnings: {:?}",
            response.warnings
        );
        assert_eq!(response.decisions[0].adr_id, "ADR-0001");
    }
}
