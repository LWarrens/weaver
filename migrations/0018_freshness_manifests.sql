-- freshness_manifests: cache + audit trail of per-view freshness manifests.
-- Never a source of truth. A new ingest does not delete rows; lookups filter on
-- repo_commit. view_hash = sha256(tool + canonical input args).

CREATE TABLE IF NOT EXISTS freshness_manifests (
    id           TEXT PRIMARY KEY NOT NULL,
    repo_id      TEXT NOT NULL REFERENCES repositories(id),
    tool         TEXT NOT NULL,
    view_hash    TEXT NOT NULL,
    repo_commit  TEXT NOT NULL,
    evaluated_at TEXT NOT NULL,
    payload      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_manifest_lookup
    ON freshness_manifests(repo_id, tool, view_hash, repo_commit);
