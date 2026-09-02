-- claims: individually verifiable assertions derived from decisions,
-- constraints, and observed ADR leads. Freshness and conflict logic operate at
-- claim granularity instead of whole-record granularity.
--
-- evidence_grade describes the GROUNDS of a claim and is set at ingest:
--   proven  - deterministic ADR parse / tree-sitter extraction / hash-verified capture
--   partial - every LLM-proposed claim; keyword-overlap link
--   unknown - a boundary crossing with no linkage evidence
-- It never enters 'proven' from model output without deterministic re-extraction.
--
-- read_set is the JSON list of artifact identities the extraction read while
-- forming the claim, so anchor completeness is checkable rather than assumed.

CREATE TABLE IF NOT EXISTS claims (
    id             TEXT PRIMARY KEY NOT NULL,
    repo_id        TEXT NOT NULL REFERENCES repositories(id),
    kind           TEXT NOT NULL,   -- decision | constraint | observation | link
    subject_type   TEXT NOT NULL,   -- decision | constraint | decision_code_link | adr_lead
    subject_id     TEXT NOT NULL,
    text           TEXT NOT NULL,
    polarity       TEXT,            -- must | must_not | null (constraint claims)
    evidence_grade TEXT NOT NULL,   -- proven | partial | unknown
    read_set       TEXT NOT NULL DEFAULT '[]',
    valid_from     TEXT NOT NULL,
    valid_to       TEXT,
    ingested_at    TEXT NOT NULL,
    source_time    TEXT,
    confidence     REAL NOT NULL DEFAULT 1.0
);

CREATE INDEX IF NOT EXISTS idx_claims_repo     ON claims(repo_id);
CREATE INDEX IF NOT EXISTS idx_claims_subject  ON claims(subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_claims_valid_to ON claims(valid_to);
