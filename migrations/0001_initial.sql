-- All timestamps stored as TEXT in ISO-8601 format (never DB-native datetime types).
-- All UUIDs stored as TEXT (lowercase hyphenated).
-- evidence_refs stored as TEXT (JSON array of UUID strings).
-- WAL mode is enabled at connection open, not here.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- repositories
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS repositories (
    id          TEXT PRIMARY KEY NOT NULL,
    path        TEXT NOT NULL UNIQUE,
    name        TEXT,
    ingested_at TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- adr_documents
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS adr_documents (
    id              TEXT PRIMARY KEY NOT NULL,
    repo_id         TEXT NOT NULL REFERENCES repositories(id),
    adr_id          TEXT NOT NULL,           -- e.g. "ADR-0042"
    title           TEXT NOT NULL,
    status          TEXT NOT NULL,           -- proposed|accepted|deprecated|superseded|rejected
    date            TEXT,                    -- date from ADR header (ISO-8601 date string)
    context         TEXT,
    decision        TEXT,
    consequences    TEXT,
    supersedes      TEXT NOT NULL DEFAULT '[]',   -- JSON array of adr_id strings
    superseded_by   TEXT,
    file_mentions   TEXT NOT NULL DEFAULT '[]',   -- JSON array of file path strings
    service_mentions TEXT NOT NULL DEFAULT '[]',
    module_mentions  TEXT NOT NULL DEFAULT '[]',
    source_uri      TEXT NOT NULL,           -- file path relative to repo root
    valid_from      TEXT NOT NULL,
    valid_to        TEXT,
    ingested_at     TEXT NOT NULL,
    source_time     TEXT,
    confidence      REAL NOT NULL DEFAULT 1.0
);

CREATE INDEX IF NOT EXISTS idx_adr_documents_repo_id  ON adr_documents(repo_id);
CREATE INDEX IF NOT EXISTS idx_adr_documents_adr_id   ON adr_documents(adr_id);
CREATE INDEX IF NOT EXISTS idx_adr_documents_status   ON adr_documents(status);
CREATE INDEX IF NOT EXISTS idx_adr_documents_valid_to ON adr_documents(valid_to);

-- ---------------------------------------------------------------------------
-- decisions
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS decisions (
    id            TEXT PRIMARY KEY NOT NULL,
    adr_id        TEXT NOT NULL REFERENCES adr_documents(id),
    text          TEXT NOT NULL,
    source_uri    TEXT NOT NULL,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    source_time   TEXT,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_decisions_adr_id   ON decisions(adr_id);
CREATE INDEX IF NOT EXISTS idx_decisions_valid_to ON decisions(valid_to);

-- ---------------------------------------------------------------------------
-- constraints
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS constraints (
    id            TEXT PRIMARY KEY NOT NULL,
    decision_id   TEXT NOT NULL REFERENCES decisions(id),
    text          TEXT NOT NULL,
    source_uri    TEXT NOT NULL,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    source_time   TEXT,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_constraints_decision_id ON constraints(decision_id);

-- ---------------------------------------------------------------------------
-- files
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS files (
    id          TEXT PRIMARY KEY NOT NULL,
    repo_id     TEXT NOT NULL REFERENCES repositories(id),
    path        TEXT NOT NULL,           -- relative to repo root
    ingested_at TEXT NOT NULL,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT,
    UNIQUE(repo_id, path)
);

CREATE INDEX IF NOT EXISTS idx_files_repo_id ON files(repo_id);
CREATE INDEX IF NOT EXISTS idx_files_path    ON files(path);

-- ---------------------------------------------------------------------------
-- symbols  (Phase 2 — tree-sitter)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS symbols (
    id          TEXT PRIMARY KEY NOT NULL,
    file_id     TEXT NOT NULL REFERENCES files(id),
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,           -- function|class|method|struct|enum|etc.
    line        INTEGER,
    ingested_at TEXT NOT NULL,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT
);

CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_name    ON symbols(name);

-- ---------------------------------------------------------------------------
-- commits  (Phase 2 — git2)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS commits (
    id            TEXT PRIMARY KEY NOT NULL,   -- UUID, not git SHA
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    sha           TEXT NOT NULL,
    author        TEXT,
    message       TEXT,
    source_time   TEXT NOT NULL,               -- commit timestamp ISO-8601
    ingested_at   TEXT NOT NULL,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    UNIQUE(repo_id, sha)
);

CREATE INDEX IF NOT EXISTS idx_commits_repo_id ON commits(repo_id);
CREATE INDEX IF NOT EXISTS idx_commits_sha     ON commits(sha);

-- ---------------------------------------------------------------------------
-- pull_requests  (Phase 2 — git2)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS pull_requests (
    id            TEXT PRIMARY KEY NOT NULL,
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    pr_ref        TEXT NOT NULL,           -- branch name or PR number
    title         TEXT,
    body          TEXT,
    source_time   TEXT,
    ingested_at   TEXT NOT NULL,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_id ON pull_requests(repo_id);

-- ---------------------------------------------------------------------------
-- episodes  (Phase 3)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS episodes (
    id            TEXT PRIMARY KEY NOT NULL,
    source        TEXT NOT NULL,           -- who/what emitted this
    source_uri    TEXT,
    content       TEXT NOT NULL,
    occurred_at   TEXT NOT NULL,
    ingested_at   TEXT NOT NULL,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_episodes_occurred_at ON episodes(occurred_at);

-- ---------------------------------------------------------------------------
-- evidence_spans
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS evidence_spans (
    id            TEXT PRIMARY KEY NOT NULL,
    source_type   TEXT NOT NULL,           -- adr|commit|episode|pr
    source_id     TEXT NOT NULL,           -- UUID of the source record
    text          TEXT,
    start_line    INTEGER,
    end_line      INTEGER,
    source_uri    TEXT NOT NULL,
    ingested_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_evidence_spans_source_id ON evidence_spans(source_id);

-- ---------------------------------------------------------------------------
-- decision_code_links
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS decision_code_links (
    id            TEXT PRIMARY KEY NOT NULL,
    decision_id   TEXT NOT NULL REFERENCES decisions(id),
    file_path     TEXT NOT NULL,           -- relative to repo root
    symbol        TEXT,
    link_type     TEXT NOT NULL,           -- mentions|applies_to|modifies
    link_source   TEXT NOT NULL,           -- adr_text|git_history|inferred
    confidence    REAL NOT NULL DEFAULT 1.0,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_dcl_decision_id ON decision_code_links(decision_id);
CREATE INDEX IF NOT EXISTS idx_dcl_file_path   ON decision_code_links(file_path);

-- ---------------------------------------------------------------------------
-- decision_git_links  (Phase 2)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS decision_git_links (
    id            TEXT PRIMARY KEY NOT NULL,
    decision_id   TEXT NOT NULL REFERENCES decisions(id),
    ref_type      TEXT NOT NULL,           -- commit|pr
    ref_id        TEXT NOT NULL,           -- UUID of commit or pr record
    link_source   TEXT NOT NULL,
    confidence    REAL NOT NULL DEFAULT 1.0,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_dgl_decision_id ON decision_git_links(decision_id);
CREATE INDEX IF NOT EXISTS idx_dgl_ref_id      ON decision_git_links(ref_id);

-- ---------------------------------------------------------------------------
-- supersession_edges
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS supersession_edges (
    id              TEXT PRIMARY KEY NOT NULL,
    superseder_id   TEXT NOT NULL REFERENCES adr_documents(id),
    superseded_id   TEXT NOT NULL REFERENCES adr_documents(id),
    valid_from      TEXT NOT NULL,
    ingested_at     TEXT NOT NULL,
    confidence      REAL NOT NULL DEFAULT 1.0,
    evidence_refs   TEXT NOT NULL DEFAULT '[]',
    UNIQUE(superseder_id, superseded_id)
);

CREATE INDEX IF NOT EXISTS idx_supersession_superseder ON supersession_edges(superseder_id);
CREATE INDEX IF NOT EXISTS idx_supersession_superseded ON supersession_edges(superseded_id);

-- ---------------------------------------------------------------------------
-- temporal_edges  (general typed graph edges)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS temporal_edges (
    id            TEXT PRIMARY KEY NOT NULL,
    edge_type     TEXT NOT NULL,           -- defines|imposes|applies_to|evidences|modifies|
                                           -- affects|supersedes|mentions|conflicts_with|
                                           -- supports|depends_on
    source_id     TEXT NOT NULL,
    source_type   TEXT NOT NULL,
    target_id     TEXT NOT NULL,
    target_type   TEXT NOT NULL,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_te_source_id  ON temporal_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_te_target_id  ON temporal_edges(target_id);
CREATE INDEX IF NOT EXISTS idx_te_edge_type  ON temporal_edges(edge_type);
CREATE INDEX IF NOT EXISTS idx_te_valid_to   ON temporal_edges(valid_to);
