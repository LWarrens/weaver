//! Claims, evidence anchors, per-anchor verifications, and index-lane freshness.
//!
//! Split out per query family; all methods extend [`SqliteStore`]. Phase 4 —
//! see `docs/DESIGN-claims-and-freshness.md`. Nothing calls these yet; claim and
//! anchor population lands with the ADR-sync and episode-ingestion changes.

#![allow(dead_code)]

use sqlx::Row;
use uuid::Uuid;

use crate::domain::entities::{
    AnchorIdentity, AnchorVerification, Claim, ClaimKind, EditClass, EvidenceAnchor, EvidenceGrade,
    Freshness, LaneRecord, Locator, Polarity,
};
use crate::error::Error;

use super::*;

fn parse_uuid(s: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(s).map_err(|e| Error::Parse(e.to_string()))
}

fn locator_to_json(loc: &Locator) -> String {
    serde_json::to_string(loc).unwrap_or_else(|_| "null".to_string())
}

fn locator_from_json(s: &str) -> Result<Locator, Error> {
    serde_json::from_str(s).map_err(|e| Error::Parse(format!("locator: {e}")))
}

fn row_to_claim(r: &sqlx::sqlite::SqliteRow) -> Result<Claim, Error> {
    let read_set_json: String = r.get("read_set");
    let read_set: Vec<AnchorIdentity> = serde_json::from_str(&read_set_json).unwrap_or_default();
    Ok(Claim {
        id: parse_uuid(r.get("id"))?,
        repo_id: parse_uuid(r.get("repo_id"))?,
        kind: ClaimKind::from_str(r.get::<&str, _>("kind")),
        subject_type: r.get("subject_type"),
        subject_id: parse_uuid(r.get("subject_id"))?,
        text: r.get("text"),
        polarity: r
            .get::<Option<String>, _>("polarity")
            .and_then(|s| Polarity::from_str(&s)),
        evidence_grade: EvidenceGrade::from_str(r.get::<&str, _>("evidence_grade")),
        read_set,
        valid_from: r.get("valid_from"),
        valid_to: r.get("valid_to"),
        ingested_at: r.get("ingested_at"),
        source_time: r.get("source_time"),
        confidence: r.get("confidence"),
    })
}

fn row_to_anchor(r: &sqlx::sqlite::SqliteRow) -> Result<EvidenceAnchor, Error> {
    Ok(EvidenceAnchor {
        id: parse_uuid(r.get("id"))?,
        repo_id: parse_uuid(r.get("repo_id"))?,
        claim_id: parse_uuid(r.get("claim_id"))?,
        identity: AnchorIdentity {
            source_kind: crate::domain::entities::AnchorSource::from_str(
                r.get::<&str, _>("source_kind"),
            ),
            source_uri: r.get("source_uri"),
            subpath: r.get("subpath"),
        },
        locator: locator_from_json(r.get::<&str, _>("locator"))?,
        anchored_text: r.get("anchored_text"),
        content_hash: r.get("content_hash"),
        context_hash: r.get("context_hash"),
        alias_of: r.get("alias_of"),
        ingested_at: r.get("ingested_at"),
        source_time: r.get("source_time"),
    })
}

fn row_to_verification(r: &sqlx::sqlite::SqliteRow) -> Result<AnchorVerification, Error> {
    let relocated: Option<String> = r.get("relocated_locator");
    Ok(AnchorVerification {
        id: parse_uuid(r.get("id"))?,
        anchor_id: parse_uuid(r.get("anchor_id"))?,
        checked_at: r.get("checked_at"),
        repo_ref: r.get("repo_ref"),
        repo_commit: r.get("repo_commit"),
        edit_class: EditClass::from_str(r.get::<&str, _>("edit_class")),
        freshness: Freshness::from_str(r.get::<&str, _>("freshness")),
        observed_hash: r.get("observed_hash"),
        relocated_locator: match relocated {
            Some(s) => Some(locator_from_json(&s)?),
            None => None,
        },
        similarity: r.get("similarity"),
        detail: r.get("detail"),
    })
}

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Claims
    // -----------------------------------------------------------------------

    /// Insert a claim. Caller owns identity and timestamps.
    pub async fn insert_claim(&self, c: &Claim) -> Result<(), Error> {
        let read_set = serde_json::to_string(&c.read_set).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            r#"INSERT INTO claims
               (id, repo_id, kind, subject_type, subject_id, text, polarity,
                evidence_grade, read_set, valid_from, valid_to, ingested_at,
                source_time, confidence)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(c.id.to_string())
        .bind(c.repo_id.to_string())
        .bind(c.kind.as_str())
        .bind(&c.subject_type)
        .bind(c.subject_id.to_string())
        .bind(&c.text)
        .bind(c.polarity.map(|p| p.as_str()))
        .bind(c.evidence_grade.as_str())
        .bind(read_set)
        .bind(&c.valid_from)
        .bind(&c.valid_to)
        .bind(&c.ingested_at)
        .bind(&c.source_time)
        .bind(c.confidence)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Open claims for a subject record.
    pub async fn claims_for_subject(
        &self,
        subject_type: &str,
        subject_id: Uuid,
    ) -> Result<Vec<Claim>, Error> {
        let rows = sqlx::query(
            r#"SELECT * FROM claims
               WHERE subject_type = ? AND subject_id = ? AND valid_to IS NULL"#,
        )
        .bind(subject_type)
        .bind(subject_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_claim).collect()
    }

    /// Open claims whose subject id is in the given set. Empty input → empty out.
    pub async fn claims_for_subjects(&self, subject_ids: &[Uuid]) -> Result<Vec<Claim>, Error> {
        if subject_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = subject_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM claims WHERE valid_to IS NULL AND subject_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql);
        for id in subject_ids {
            q = q.bind(id.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_claim).collect()
    }

    /// Every open claim for a repository. Used by the integrity oracle.
    pub async fn open_claims_for_repo(&self, repo_id: Uuid) -> Result<Vec<Claim>, Error> {
        let rows = sqlx::query(
            "SELECT * FROM claims WHERE repo_id = ? AND valid_to IS NULL",
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_claim).collect()
    }

    /// Fetch claims by their own ids (open or closed). Empty input → empty out.
    pub async fn claims_by_ids(&self, claim_ids: &[Uuid]) -> Result<Vec<Claim>, Error> {
        if claim_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = claim_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT * FROM claims WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in claim_ids {
            q = q.bind(id.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_claim).collect()
    }

    /// Open claim ids (decision + constraint) belonging to a set of decisions.
    pub async fn claim_ids_for_decisions(
        &self,
        decision_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, Error> {
        if decision_ids.is_empty() {
            return Ok(vec![]);
        }
        let ph = decision_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            r#"SELECT id FROM claims WHERE valid_to IS NULL AND (
                 (subject_type = 'decision' AND subject_id IN ({ph}))
                 OR (subject_type = 'constraint' AND subject_id IN
                     (SELECT id FROM constraints WHERE decision_id IN ({ph})))
               )"#
        );
        let mut q = sqlx::query(&sql);
        for id in decision_ids {
            q = q.bind(id.to_string());
        }
        for id in decision_ids {
            q = q.bind(id.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| parse_uuid(r.get("id")))
            .collect()
    }

    /// Open claim ids for every decision under an ADR document.
    pub async fn claim_ids_for_adr_doc(&self, adr_doc_id: Uuid) -> Result<Vec<Uuid>, Error> {
        let rows = sqlx::query(
            r#"SELECT id FROM claims WHERE valid_to IS NULL AND (
                 (subject_type = 'decision' AND subject_id IN
                    (SELECT id FROM decisions WHERE adr_id = ?))
                 OR (subject_type = 'constraint' AND subject_id IN
                    (SELECT id FROM constraints WHERE decision_id IN
                       (SELECT id FROM decisions WHERE adr_id = ?)))
               )"#,
        )
        .bind(adr_doc_id.to_string())
        .bind(adr_doc_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| parse_uuid(r.get("id"))).collect()
    }

    /// The governing decision id and human ADR id for a claim subject.
    /// `(decision_id, adr_id)` — both `None` when the subject cannot be resolved.
    pub async fn decision_and_adr_for_subject(
        &self,
        subject_type: &str,
        subject_id: Uuid,
    ) -> Result<(Option<String>, Option<String>), Error> {
        let sql = match subject_type {
            "decision" => {
                r#"SELECT d.id AS decision_id, a.adr_id AS adr_id
                   FROM decisions d LEFT JOIN adr_documents a ON a.id = d.adr_id
                   WHERE d.id = ?"#
            }
            "constraint" => {
                r#"SELECT d.id AS decision_id, a.adr_id AS adr_id
                   FROM constraints c JOIN decisions d ON d.id = c.decision_id
                   LEFT JOIN adr_documents a ON a.id = d.adr_id
                   WHERE c.id = ?"#
            }
            _ => return Ok((None, None)),
        };
        let row = sqlx::query(sql)
            .bind(subject_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => (r.get("decision_id"), r.get("adr_id")),
            None => (None, None),
        })
    }

    /// Close every open claim for a subject (retraction cascade).
    pub async fn close_claims_for_subject(
        &self,
        subject_type: &str,
        subject_id: Uuid,
        valid_to: &str,
    ) -> Result<u64, Error> {
        let res = sqlx::query(
            r#"UPDATE claims SET valid_to = ?
               WHERE subject_type = ? AND subject_id = ? AND valid_to IS NULL"#,
        )
        .bind(valid_to)
        .bind(subject_type)
        .bind(subject_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    // -----------------------------------------------------------------------
    // Evidence anchors
    // -----------------------------------------------------------------------

    /// Insert an anchor. Idempotent on
    /// `(claim_id, source_kind, source_uri, subpath, content_hash)`.
    pub async fn insert_evidence_anchor(&self, a: &EvidenceAnchor) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO evidence_anchors
               (id, repo_id, claim_id, source_kind, source_uri, subpath, locator,
                anchored_text, content_hash, context_hash, alias_of, ingested_at,
                source_time)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(a.id.to_string())
        .bind(a.repo_id.to_string())
        .bind(a.claim_id.to_string())
        .bind(a.identity.source_kind.as_str())
        .bind(&a.identity.source_uri)
        .bind(&a.identity.subpath)
        .bind(locator_to_json(&a.locator))
        .bind(&a.anchored_text)
        .bind(&a.content_hash)
        .bind(&a.context_hash)
        .bind(&a.alias_of)
        .bind(&a.ingested_at)
        .bind(&a.source_time)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Anchors for one claim.
    pub async fn anchors_for_claim(&self, claim_id: Uuid) -> Result<Vec<EvidenceAnchor>, Error> {
        let rows = sqlx::query("SELECT * FROM evidence_anchors WHERE claim_id = ?")
            .bind(claim_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_anchor).collect()
    }

    /// Every anchor for a repository whose owning claim is still open, capped at
    /// `limit`. Used to warm the verification cache after re-ingest.
    pub async fn open_anchors_for_repo(
        &self,
        repo_id: Uuid,
        limit: i64,
    ) -> Result<Vec<EvidenceAnchor>, Error> {
        let rows = sqlx::query(
            r#"SELECT a.* FROM evidence_anchors a
               JOIN claims c ON c.id = a.claim_id
               WHERE a.repo_id = ? AND c.valid_to IS NULL
               LIMIT ?"#,
        )
        .bind(repo_id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_anchor).collect()
    }

    /// Anchors for a set of claims. Empty input → empty out.
    pub async fn anchors_for_claims(
        &self,
        claim_ids: &[Uuid],
    ) -> Result<Vec<EvidenceAnchor>, Error> {
        if claim_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = claim_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql =
            format!("SELECT * FROM evidence_anchors WHERE claim_id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in claim_ids {
            q = q.bind(id.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_anchor).collect()
    }

    // -----------------------------------------------------------------------
    // Verifications  (append-only)
    // -----------------------------------------------------------------------

    /// Append a verification row.
    pub async fn insert_anchor_verification(
        &self,
        v: &AnchorVerification,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT INTO evidence_verifications
               (id, anchor_id, checked_at, repo_ref, repo_commit, edit_class,
                freshness, observed_hash, relocated_locator, similarity, detail)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(v.id.to_string())
        .bind(v.anchor_id.to_string())
        .bind(&v.checked_at)
        .bind(&v.repo_ref)
        .bind(&v.repo_commit)
        .bind(v.edit_class.as_str())
        .bind(v.freshness.as_str())
        .bind(&v.observed_hash)
        .bind(v.relocated_locator.as_ref().map(locator_to_json))
        .bind(v.similarity)
        .bind(&v.detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most recent verification of an anchor against a resolved commit.
    pub async fn latest_verification(
        &self,
        anchor_id: Uuid,
        repo_commit: &str,
    ) -> Result<Option<AnchorVerification>, Error> {
        let row = sqlx::query(
            r#"SELECT * FROM evidence_verifications
               WHERE anchor_id = ? AND repo_commit = ?
               ORDER BY checked_at DESC LIMIT 1"#,
        )
        .bind(anchor_id.to_string())
        .bind(repo_commit)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_verification).transpose()
    }

    // -----------------------------------------------------------------------
    // Index lanes
    // -----------------------------------------------------------------------

    /// Record (upsert) the freshness of one index lane.
    pub async fn upsert_index_lane(
        &self,
        repo_id: Uuid,
        lane: &str,
        last_ingested_commit: Option<&str>,
        last_ingested_at: &str,
        status: &str,
        detail: Option<&str>,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT INTO index_lanes
               (repo_id, lane, last_ingested_commit, last_ingested_at, status, detail)
               VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT(repo_id, lane) DO UPDATE SET
                 last_ingested_commit = excluded.last_ingested_commit,
                 last_ingested_at     = excluded.last_ingested_at,
                 status               = excluded.status,
                 detail               = excluded.detail"#,
        )
        .bind(repo_id.to_string())
        .bind(lane)
        .bind(last_ingested_commit)
        .bind(last_ingested_at)
        .bind(status)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All recorded index lanes for a repository.
    pub async fn index_lanes(&self, repo_id: Uuid) -> Result<Vec<LaneRecord>, Error> {
        let rows = sqlx::query(
            r#"SELECT lane, last_ingested_commit, last_ingested_at, status, detail
               FROM index_lanes WHERE repo_id = ? ORDER BY lane"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| LaneRecord {
                lane: r.get("lane"),
                last_ingested_commit: r.get("last_ingested_commit"),
                last_ingested_at: r.get("last_ingested_at"),
                status: r.get("status"),
                detail: r.get("detail"),
            })
            .collect())
    }

    /// Decisions whose evidence anchors have drifted: at least one open
    /// decision-subject claim has an anchor whose most recent verification is
    /// `stale`. Row: `(decision_id, adr_id, title, valid_from, stale_anchors,
    /// unrelocated_stale_anchors)`. `unrelocated_stale_anchors == 0` means every
    /// drifted anchor was relocated (claim is *affected*, not *unprovable*).
    pub async fn drifted_evidence_decisions(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String, String, i64, i64)>, Error> {
        let rows = sqlx::query(
            r#"WITH latest AS (
                 SELECT ev.anchor_id, ev.freshness, ev.relocated_locator
                 FROM evidence_verifications ev
                 JOIN (SELECT anchor_id, MAX(checked_at) AS mx
                       FROM evidence_verifications GROUP BY anchor_id) m
                   ON m.anchor_id = ev.anchor_id AND m.mx = ev.checked_at
               )
               SELECT d.id AS decision_id,
                      COALESCE(a.adr_id, '') AS adr_id,
                      COALESCE(d.title, '') AS title,
                      d.valid_from AS valid_from,
                      SUM(CASE WHEN latest.freshness = 'stale' THEN 1 ELSE 0 END) AS stale_n,
                      SUM(CASE WHEN latest.freshness = 'stale'
                                AND latest.relocated_locator IS NULL
                               THEN 1 ELSE 0 END) AS unreloc_n
               FROM claims cl
               JOIN evidence_anchors ea ON ea.claim_id = cl.id
               JOIN latest ON latest.anchor_id = ea.id
               JOIN decisions d ON d.id = cl.subject_id
               LEFT JOIN adr_documents a ON a.id = d.adr_id
               WHERE cl.repo_id = ? AND cl.valid_to IS NULL
                 AND cl.subject_type = 'decision'
               GROUP BY d.id
               HAVING stale_n > 0"#,
        )
        .bind(repo_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("decision_id"),
                    r.get::<String, _>("adr_id"),
                    r.get::<String, _>("title"),
                    r.get::<String, _>("valid_from"),
                    r.get::<i64, _>("stale_n"),
                    r.get::<i64, _>("unreloc_n"),
                )
            })
            .collect())
    }

    // -----------------------------------------------------------------------
    // Freshness manifest cache
    // -----------------------------------------------------------------------

    /// Cache a computed manifest payload for a view at a commit.
    pub async fn store_freshness_manifest(
        &self,
        repo_id: Uuid,
        tool: &str,
        view_hash: &str,
        repo_commit: &str,
        evaluated_at: &str,
        payload: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"INSERT INTO freshness_manifests
               (id, repo_id, tool, view_hash, repo_commit, evaluated_at, payload)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(repo_id.to_string())
        .bind(tool)
        .bind(view_hash)
        .bind(repo_commit)
        .bind(evaluated_at)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most recent cached manifest payload for a view at a commit.
    pub async fn cached_freshness_manifest(
        &self,
        repo_id: Uuid,
        tool: &str,
        view_hash: &str,
        repo_commit: &str,
    ) -> Result<Option<String>, Error> {
        let row = sqlx::query(
            r#"SELECT payload FROM freshness_manifests
               WHERE repo_id = ? AND tool = ? AND view_hash = ? AND repo_commit = ?
               ORDER BY evaluated_at DESC LIMIT 1"#,
        )
        .bind(repo_id.to_string())
        .bind(tool)
        .bind(view_hash)
        .bind(repo_commit)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<String, _>("payload")))
    }
}
