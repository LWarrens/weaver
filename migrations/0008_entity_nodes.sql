-- entity_nodes: canonical entity identity and resolution
-- Entities resolve mentions from ADRs, episodes, and decision context to stable identities.
-- Used for linking decisions to entities (services, modules, components) and
-- aggregating decisions across naming variations ("auth-svc" vs "auth service").

CREATE TABLE IF NOT EXISTS entity_nodes (
    id             TEXT PRIMARY KEY NOT NULL,
    repo_id        TEXT NOT NULL REFERENCES repositories(id),
    canonical_name TEXT NOT NULL,
    entity_type    TEXT,
    embedding      BLOB,
    confidence     REAL NOT NULL DEFAULT 0.7,
    valid_from     TEXT NOT NULL,
    valid_to       TEXT,
    ingested_at    TEXT NOT NULL,
    evidence_refs  TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_entity_nodes_repo  ON entity_nodes(repo_id);
CREATE INDEX IF NOT EXISTS idx_entity_nodes_name  ON entity_nodes(canonical_name);
CREATE INDEX IF NOT EXISTS idx_entity_nodes_valid ON entity_nodes(valid_to);
