//! Check whether a claim's anchors cover every artifact its extraction read.
//!
//! Pure. If a read artifact has no anchor, its drift would produce no freshness
//! signal — so incompleteness is surfaced rather than assumed away.

use crate::domain::entities::{AnchorIdentity, EvidenceAnchor};

/// True when `anchor` covers `want` under container/entry subsumption: same
/// `(source_kind, source_uri)`, and the anchor's subpath is empty (covers the
/// whole artifact) or equal to the wanted subpath.
fn covers(anchor: &AnchorIdentity, want: &AnchorIdentity) -> bool {
    anchor.source_kind == want.source_kind
        && anchor.source_uri == want.source_uri
        && (anchor.subpath.is_empty() || anchor.subpath == want.subpath)
}

/// Read-set identities not covered by any of the claim's anchors.
pub fn uncovered(read_set: &[AnchorIdentity], anchors: &[EvidenceAnchor]) -> Vec<AnchorIdentity> {
    read_set
        .iter()
        .filter(|want| !anchors.iter().any(|a| covers(&a.identity, want)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::{AnchorSource, Locator};
    use uuid::Uuid;

    fn anchor(kind: AnchorSource, uri: &str, subpath: &str) -> EvidenceAnchor {
        EvidenceAnchor {
            id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            claim_id: Uuid::new_v4(),
            identity: AnchorIdentity {
                source_kind: kind,
                source_uri: uri.to_string(),
                subpath: subpath.to_string(),
            },
            locator: Locator::Section {
                name: "Decision".to_string(),
            },
            anchored_text: String::new(),
            content_hash: String::new(),
            context_hash: None,
            alias_of: None,
            ingested_at: String::new(),
            source_time: None,
        }
    }

    fn ident(kind: AnchorSource, uri: &str, subpath: &str) -> AnchorIdentity {
        AnchorIdentity {
            source_kind: kind,
            source_uri: uri.to_string(),
            subpath: subpath.to_string(),
        }
    }

    #[test]
    fn whole_artifact_anchor_covers_subpath() {
        let anchors = vec![anchor(AnchorSource::Adr, "ADR-0042", "")];
        let read = vec![ident(AnchorSource::Adr, "ADR-0042", "Decision")];
        assert!(uncovered(&read, &anchors).is_empty());
    }

    #[test]
    fn missing_read_artifact_is_reported() {
        let anchors = vec![anchor(AnchorSource::Adr, "ADR-0042", "Decision")];
        let read = vec![
            ident(AnchorSource::Adr, "ADR-0042", "Decision"),
            ident(AnchorSource::SourceFile, "src/orders/events.rs", ""),
        ];
        let missing = uncovered(&read, &anchors);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].source_uri, "src/orders/events.rs");
    }
}
