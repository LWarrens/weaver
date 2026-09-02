-- symbol_edges: directed call/import/inheritance relationships between symbols
CREATE TABLE IF NOT EXISTS symbol_edges (
    id          TEXT PRIMARY KEY NOT NULL,
    repo_id     TEXT NOT NULL REFERENCES repositories(id),
    from_id     TEXT NOT NULL REFERENCES symbols(id),
    to_id       TEXT REFERENCES symbols(id),   -- NULL when callee is unresolved
    to_name     TEXT,                           -- raw callee name for unresolved refs
    edge_type   TEXT NOT NULL CHECK(edge_type IN
                  ('calls','imports','inherits','implements','uses_type')),
    confidence  REAL NOT NULL DEFAULT 1.0,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT
);

CREATE INDEX IF NOT EXISTS idx_symbol_edges_from ON symbol_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_symbol_edges_to   ON symbol_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_symbol_edges_repo  ON symbol_edges(repo_id);
