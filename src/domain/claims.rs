//! Decompose decisions, constraints, and observed leads into individually
//! verifiable claims, and detect constraint obligation polarity.
//!
//! Pure. The evidence grade is decided by the *caller* from how the fact was
//! extracted (deterministic parse vs. LLM), never inferred here.

use uuid::Uuid;

use crate::domain::entities::{AnchorIdentity, Claim, ClaimKind, EvidenceGrade, Polarity};

/// Detect obligation polarity from constraint text.
///
/// Negated obligations ("must not", "never", "shall not", "prohibited",
/// "cannot", "may not") → `MustNot`. Plain obligations ("must", "shall",
/// "always", "required") → `Must`. Anything else → `None`.
pub fn detect_polarity(text: &str) -> Option<Polarity> {
    let t = text.to_lowercase();
    const NEG: &[&str] = &[
        "must not",
        "must never",
        "shall not",
        "may not",
        "cannot ",
        "can not ",
        "never ",
        "prohibit",
        "forbidden",
        "disallow",
        "not be allowed",
    ];
    const POS: &[&str] = &[
        "must ",
        "shall ",
        "always ",
        "required to",
        "is required",
        "are required",
        "has to ",
        "have to ",
    ];
    if NEG.iter().any(|k| t.contains(k)) {
        return Some(Polarity::MustNot);
    }
    if POS.iter().any(|k| t.contains(k)) {
        return Some(Polarity::Must);
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn claim(
    repo_id: Uuid,
    kind: ClaimKind,
    subject_type: &str,
    subject_id: Uuid,
    text: impl Into<String>,
    polarity: Option<Polarity>,
    grade: EvidenceGrade,
    read_set: Vec<AnchorIdentity>,
    now: &str,
    source_time: Option<String>,
    confidence: f64,
) -> Claim {
    Claim {
        id: Uuid::new_v4(),
        repo_id,
        kind,
        subject_type: subject_type.to_string(),
        subject_id,
        text: text.into(),
        polarity,
        evidence_grade: grade,
        read_set,
        valid_from: now.to_string(),
        valid_to: None,
        ingested_at: now.to_string(),
        source_time,
        confidence,
    }
}

/// One `decision` claim for a decision record.
pub fn decision_claim(
    repo_id: Uuid,
    decision_id: Uuid,
    text: &str,
    grade: EvidenceGrade,
    read_set: Vec<AnchorIdentity>,
    now: &str,
    source_time: Option<String>,
    confidence: f64,
) -> Claim {
    claim(
        repo_id,
        ClaimKind::Decision,
        "decision",
        decision_id,
        text,
        None,
        grade,
        read_set,
        now,
        source_time,
        confidence,
    )
}

/// One `constraint` claim for a constraint record, polarity auto-detected.
pub fn constraint_claim(
    repo_id: Uuid,
    constraint_id: Uuid,
    text: &str,
    grade: EvidenceGrade,
    read_set: Vec<AnchorIdentity>,
    now: &str,
    source_time: Option<String>,
    confidence: f64,
) -> Claim {
    claim(
        repo_id,
        ClaimKind::Constraint,
        "constraint",
        constraint_id,
        text,
        detect_polarity(text),
        grade,
        read_set,
        now,
        source_time,
        confidence,
    )
}

/// One `observation` claim for an observed ADR lead.
pub fn observation_claim(
    repo_id: Uuid,
    lead_subject_id: Uuid,
    text: &str,
    read_set: Vec<AnchorIdentity>,
    now: &str,
    confidence: f64,
) -> Claim {
    claim(
        repo_id,
        ClaimKind::Observation,
        "adr_lead",
        lead_subject_id,
        text,
        None,
        EvidenceGrade::Partial,
        read_set,
        now,
        None,
        confidence,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polarity_detection() {
        assert_eq!(
            detect_polarity("Order state must never be mutated in place"),
            Some(Polarity::MustNot)
        );
        assert_eq!(
            detect_polarity("Queries must use parameterized statements"),
            Some(Polarity::Must)
        );
        assert_eq!(
            detect_polarity("Writes are prohibited on replicas"),
            Some(Polarity::MustNot)
        );
        assert_eq!(detect_polarity("The service is event driven"), None);
    }
}
