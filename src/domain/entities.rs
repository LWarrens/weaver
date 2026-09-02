use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// ADR status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AdrStatus {
    Proposed,
    Accepted,
    Deprecated,
    Superseded,
    Rejected,
    /// Unknown status string preserved as-is
    Unknown,
}

impl AdrStatus {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "proposed" => AdrStatus::Proposed,
            "accepted" => AdrStatus::Accepted,
            "deprecated" => AdrStatus::Deprecated,
            "superseded" => AdrStatus::Superseded,
            "rejected" => AdrStatus::Rejected,
            _ => AdrStatus::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AdrStatus::Proposed => "proposed",
            AdrStatus::Accepted => "accepted",
            AdrStatus::Deprecated => "deprecated",
            AdrStatus::Superseded => "superseded",
            AdrStatus::Rejected => "rejected",
            AdrStatus::Unknown => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: Uuid,
    pub path: String,
    pub name: Option<String>,
    pub ingested_at: String,
}

// ---------------------------------------------------------------------------
// ADR document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdrDocument {
    pub id: Uuid,
    pub repo_id: Uuid,
    /// Human-readable ADR identifier, e.g. "ADR-0042"
    pub adr_id: String,
    pub title: String,
    pub status: AdrStatus,
    /// Date string from the ADR header (ISO-8601 date)
    pub date: Option<String>,
    pub context: Option<String>,
    pub decision: Option<String>,
    pub consequences: Option<String>,
    /// ADR IDs this document supersedes
    pub supersedes: Vec<String>,
    /// ADR ID this document is superseded by
    pub superseded_by: Option<String>,
    /// File paths explicitly mentioned in the ADR text
    pub file_mentions: Vec<String>,
    pub service_mentions: Vec<String>,
    pub module_mentions: Vec<String>,
    /// Path to the markdown file, relative to the repository root
    pub source_uri: String,
    /// Event time: when this ADR's decision became architecturally effective.
    pub effective_from: Option<String>,
    /// Event time: when this ADR's decision stopped being architecturally effective.
    pub effective_to: Option<String>,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub source_time: Option<String>,
    pub confidence: f64,
}

// ---------------------------------------------------------------------------
// Entity Node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityNode {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub canonical_name: String,
    pub entity_type: Option<String>,
    pub confidence: f64,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub evidence_refs: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: Uuid,
    pub title: Option<String>,
    /// FK to adr_documents.id for ADR-sourced decisions.
    pub adr_id: Option<Uuid>,
    /// FK to episodes.id for episode-sourced decisions.
    pub episode_id: Option<Uuid>,
    pub text: String,
    pub source_uri: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub source_time: Option<String>,
    pub confidence: f64,
    pub evidence_refs: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// Constraint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    pub id: Uuid,
    pub decision_id: Uuid,
    pub text: String,
    pub source_uri: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub source_time: Option<String>,
    pub confidence: f64,
    pub evidence_refs: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// Decision ↔ code file link
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    Mentions,
    AppliesTo,
    Modifies,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkSource {
    /// Extracted from ADR text file mentions
    AdrText,
    /// From git history (Phase 2)
    GitHistory,
    /// Heuristic inference — always low confidence
    Inferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionCodeLink {
    pub id: Uuid,
    pub decision_id: Uuid,
    /// File path relative to repository root
    pub file_path: String,
    pub symbol: Option<String>,
    pub link_type: LinkType,
    pub link_source: LinkSource,
    pub confidence: f64,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub evidence_refs: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// Common tool response
// ---------------------------------------------------------------------------

/// Shape returned by every tool that resolves architectural knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchResponse {
    pub answer: Option<String>,
    pub entities: Vec<serde_json::Value>,
    pub decisions: Vec<DecisionSummary>,
    pub constraints: Vec<ConstraintSummary>,
    pub evidence: Vec<serde_json::Value>,
    pub facts_extracted: usize,
    pub warnings: Vec<String>,
    pub conflicts: Vec<serde_json::Value>,
    pub temporal_context: TemporalContext,
    pub confidence: f64,
    /// Per-view freshness of the evidence backing the decisions above.
    /// `None` when `verify: skip` or the tool returned no decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<FreshnessManifest>,
    /// Present instead of a usable answer when `verify: strict` found a stale
    /// claim on the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<RebuildObligation>,
}

impl ArchResponse {
    pub fn empty() -> Self {
        ArchResponse {
            answer: None,
            entities: vec![],
            decisions: vec![],
            constraints: vec![],
            evidence: vec![],
            facts_extracted: 0,
            warnings: vec![],
            conflicts: vec![],
            temporal_context: TemporalContext {
                valid_at: None,
                ingested_at: None,
            },
            confidence: 0.0,
            freshness: None,
            refused: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionSummary {
    pub id: String,
    pub adr_id: String,
    pub episode_id: Option<String>,
    pub title: String,
    pub status: String,
    pub text: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintSummary {
    pub id: String,
    pub decision_id: String,
    pub text: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalContext {
    pub valid_at: Option<String>,
    pub ingested_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemporalMode {
    Event,
    Ingestion,
}

impl Default for TemporalMode {
    fn default() -> Self {
        TemporalMode::Event
    }
}

// ---------------------------------------------------------------------------
// Claims, evidence anchors, verifications  (Phase 4)
// ---------------------------------------------------------------------------

/// What kind of record a claim was decomposed from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Decision,
    Constraint,
    Observation,
    Link,
}

impl ClaimKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaimKind::Decision => "decision",
            ClaimKind::Constraint => "constraint",
            ClaimKind::Observation => "observation",
            ClaimKind::Link => "link",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "constraint" => ClaimKind::Constraint,
            "observation" => ClaimKind::Observation,
            "link" => ClaimKind::Link,
            _ => ClaimKind::Decision,
        }
    }
}

/// The grounds a claim rests on, independent of freshness. Ordered:
/// `Unknown < Partial < Proven`. Model output never enters at `Proven`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceGrade {
    Unknown,
    Partial,
    Proven,
}

impl EvidenceGrade {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceGrade::Unknown => "unknown",
            EvidenceGrade::Partial => "partial",
            EvidenceGrade::Proven => "proven",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "proven" => EvidenceGrade::Proven,
            "partial" => EvidenceGrade::Partial,
            _ => EvidenceGrade::Unknown,
        }
    }
}

/// Obligation polarity carried by constraint claims.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    Must,
    MustNot,
}

impl Polarity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Polarity::Must => "must",
            Polarity::MustNot => "must_not",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "must" => Some(Polarity::Must),
            "must_not" => Some(Polarity::MustNot),
            _ => None,
        }
    }
}

/// The kind of artifact an anchor cites.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnchorSource {
    Adr,
    Episode,
    Commit,
    Pr,
    SourceFile,
    Symbol,
}

impl AnchorSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnchorSource::Adr => "adr",
            AnchorSource::Episode => "episode",
            AnchorSource::Commit => "commit",
            AnchorSource::Pr => "pr",
            AnchorSource::SourceFile => "source_file",
            AnchorSource::Symbol => "symbol",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "adr" => AnchorSource::Adr,
            "episode" => AnchorSource::Episode,
            "commit" => AnchorSource::Commit,
            "pr" => AnchorSource::Pr,
            "symbol" => AnchorSource::Symbol,
            _ => AnchorSource::SourceFile,
        }
    }
}

/// Binary freshness of a single anchor against a resolved commit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
}

impl Freshness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Freshness::Fresh => "fresh",
            Freshness::Stale => "stale",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "stale" => Freshness::Stale,
            _ => Freshness::Fresh,
        }
    }
}

/// How an anchored span changed between ingest and the verification commit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditClass {
    Unchanged,
    Shifted,
    Affected,
    Deleted,
}

impl EditClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            EditClass::Unchanged => "unchanged",
            EditClass::Shifted => "shifted",
            EditClass::Affected => "affected",
            EditClass::Deleted => "deleted",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "shifted" => EditClass::Shifted,
            "affected" => EditClass::Affected,
            "deleted" => EditClass::Deleted,
            _ => EditClass::Unchanged,
        }
    }
}

/// Derived three-state disposition of a claim over its anchors' verifications.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Unaffected,
    Affected,
    Unprovable,
}

impl Disposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Unaffected => "unaffected",
            Disposition::Affected => "affected",
            Disposition::Unprovable => "unprovable",
        }
    }
}

/// Where inside a source artifact an anchor points.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Locator {
    Lines { start: u32, end: u32 },
    SymbolQn { qn: String },
    Section { name: String },
    Chars { start: usize, end: usize },
}

/// Canonical, alias-resolved identity of an anchored artifact span.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnchorIdentity {
    pub source_kind: AnchorSource,
    pub source_uri: String,
    #[serde(default)]
    pub subpath: String,
}

/// An individually verifiable assertion hanging off a decision, constraint, or
/// observed lead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub kind: ClaimKind,
    pub subject_type: String,
    pub subject_id: Uuid,
    pub text: String,
    pub polarity: Option<Polarity>,
    pub evidence_grade: EvidenceGrade,
    pub read_set: Vec<AnchorIdentity>,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub source_time: Option<String>,
    pub confidence: f64,
}

/// A claim's immutable citation of an exact source span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceAnchor {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub claim_id: Uuid,
    pub identity: AnchorIdentity,
    pub locator: Locator,
    pub anchored_text: String,
    pub content_hash: String,
    pub context_hash: Option<String>,
    pub alias_of: Option<String>,
    pub ingested_at: String,
    pub source_time: Option<String>,
}

/// One append-only check of an anchor against a resolved commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorVerification {
    pub id: Uuid,
    pub anchor_id: Uuid,
    pub checked_at: String,
    pub repo_ref: String,
    pub repo_commit: String,
    pub edit_class: EditClass,
    pub freshness: Freshness,
    pub observed_hash: Option<String>,
    pub relocated_locator: Option<Locator>,
    pub similarity: Option<f64>,
    pub detail: Option<String>,
}

/// Freshness of one index lane for a repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneRecord {
    pub lane: String,
    pub last_ingested_commit: Option<String>,
    pub last_ingested_at: String,
    pub status: String,
    pub detail: Option<String>,
}

/// One index lane's status inside a freshness manifest, with derived lag and
/// the query capabilities it enables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneStatus {
    pub lane: String,
    pub last_ingested_commit: Option<String>,
    pub lag_commits: Option<u32>,
    pub status: String,
    pub capabilities: Vec<String>,
}

/// One offending anchor inside a stale claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleAnchorDetail {
    pub anchor_id: String,
    pub identity: AnchorIdentity,
    pub edit_class: String,
    pub freshness: String,
    pub relocated_locator: Option<Locator>,
    pub detail: Option<String>,
}

/// A claim whose disposition is not `unaffected`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleClaim {
    pub claim_id: String,
    pub subject_type: String,
    pub subject_id: String,
    pub decision_id: Option<String>,
    pub adr_id: Option<String>,
    pub text: String,
    pub evidence_grade: String,
    pub disposition: String,
    pub anchors: Vec<StaleAnchorDetail>,
}

/// A claim whose recorded read-set is not fully covered by anchors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteClaim {
    pub claim_id: String,
    pub text: String,
    pub uncovered: Vec<AnchorIdentity>,
}

/// Per-view freshness summary attached to a retrieval response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreshnessManifest {
    pub evaluated_at: String,
    pub repo_ref: String,
    pub repo_commit: String,
    pub anchors_total: usize,
    pub by_disposition: std::collections::BTreeMap<String, usize>,
    pub stale_claims: Vec<StaleClaim>,
    pub incomplete_claims: Vec<IncompleteClaim>,
    pub lanes: Vec<LaneStatus>,
    pub warnings: Vec<String>,
}

/// Returned instead of an answer when `verify: strict` finds a stale claim on
/// the path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildObligation {
    pub reason: String,
    pub drifted_anchors: Vec<String>,
    pub commands: Vec<String>,
}
