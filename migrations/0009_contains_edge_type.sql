-- Add 'contains' to symbol_edges.edge_type constraint for structural nesting.
-- SQLite does not support ALTER TABLE to change constraints, so we recreate the table.
CREATE TABLE symbol_edges_new (
    id          TEXT PRIMARY KEY NOT NULL,
    repo_id     TEXT NOT NULL REFERENCES repositories(id),
    from_id     TEXT NOT NULL REFERENCES symbols(id),
    to_id       TEXT REFERENCES symbols(id),
    to_name     TEXT,
    edge_type   TEXT NOT NULL CHECK(edge_type IN
                  ('calls','imports','inherits','implements','uses_type','contains')),
    confidence  REAL NOT NULL DEFAULT 1.0,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT
);

INSERT INTO symbol_edges_new SELECT * FROM symbol_edges;
DROP TABLE symbol_edges;
ALTER TABLE symbol_edges_new RENAME TO symbol_edges;

CREATE INDEX IF NOT EXISTS idx_symbol_edges_from ON symbol_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_symbol_edges_to   ON symbol_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_symbol_edges_repo  ON symbol_edges(repo_id);
