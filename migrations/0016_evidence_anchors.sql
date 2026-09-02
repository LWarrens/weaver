-- evidence_anchors: a claim's citation of an exact source span.
--
-- Identity is the canonical alias-resolved triple (source_kind, source_uri,
-- subpath). subpath resolves through re-exports / entity_nodes to the LEAF
-- definition before hashing, so a re-exported symbol is not a false alarm.
--
--   content_hash - sha256 of the whitespace-normalized anchored_text (the span,
--                  not the whole file), so sub-path drift is what's detected.
--   context_hash - sha256 of the normalized enclosing +/- 8 lines, used to
--                  relocate a span whose exact text moved.
--   alias_of     - the pre-resolution identity string, when it differed.
--
-- Anchors are immutable once written; a changed citation is a new anchor.
-- evidence_spans (migration 0001, never populated) is superseded by this table
-- and dropped in a later migration once ADR sync + episode ingestion populate
-- claims and anchors.

CREATE TABLE IF NOT EXISTS evidence_anchors (
    id            TEXT PRIMARY KEY NOT NULL,
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    claim_id      TEXT NOT NULL REFERENCES claims(id),
    source_kind   TEXT NOT NULL,   -- adr | episode | commit | pr | source_file | symbol
    source_uri    TEXT NOT NULL,
    subpath       TEXT NOT NULL DEFAULT '',
    locator       TEXT NOT NULL,   -- JSON: {"lines":{...}} | {"symbol_qn":{...}} | {"section":{...}} | {"chars":{...}}
    anchored_text TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    context_hash  TEXT,
    alias_of      TEXT,
    ingested_at   TEXT NOT NULL,
    source_time   TEXT,
    UNIQUE(claim_id, source_kind, source_uri, subpath, content_hash)
);

CREATE INDEX IF NOT EXISTS idx_anchors_claim ON evidence_anchors(claim_id);
CREATE INDEX IF NOT EXISTS idx_anchors_uri   ON evidence_anchors(repo_id, source_uri);
CREATE INDEX IF NOT EXISTS idx_anchors_chash ON evidence_anchors(content_hash);

-- evidence_verifications: append-only record of one anchor checked against one
-- resolved commit. Never mutates the anchor or the claim.
--
--   edit_class - unchanged | shifted | affected | deleted  (CodeNib vocabulary)
--   freshness  - fresh | stale
--   relocated_locator - present when the span moved (shifted) or was fuzzily
--                       re-found (affected)
--   similarity - token-overlap ratio for an affected-relocated span

CREATE TABLE IF NOT EXISTS evidence_verifications (
    id                TEXT PRIMARY KEY NOT NULL,
    anchor_id         TEXT NOT NULL REFERENCES evidence_anchors(id),
    checked_at        TEXT NOT NULL,
    repo_ref          TEXT NOT NULL,   -- ref name as requested
    repo_commit       TEXT NOT NULL,   -- resolved SHA (the cache key)
    edit_class        TEXT NOT NULL,   -- unchanged | shifted | affected | deleted
    freshness         TEXT NOT NULL,   -- fresh | stale
    observed_hash     TEXT,
    relocated_locator TEXT,
    similarity        REAL,
    detail            TEXT
);

CREATE INDEX IF NOT EXISTS idx_verif_anchor ON evidence_verifications(anchor_id);
CREATE INDEX IF NOT EXISTS idx_verif_commit ON evidence_verifications(anchor_id, repo_commit);
