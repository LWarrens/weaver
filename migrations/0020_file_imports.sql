-- Phase 5: scoped call-graph resolution.
--
-- file_imports: per-file resolved import table. Each row says "file `from_file_id`
-- brought name `imported_name` into scope, and it resolves to file `to_file_id`
-- (and, when known, symbol `to_symbol_id`)". Built once per ingest so call
-- resolution and sync_incremental can reuse it instead of re-guessing module paths.
CREATE TABLE IF NOT EXISTS file_imports (
    id            TEXT PRIMARY KEY NOT NULL,
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    from_file_id  TEXT NOT NULL REFERENCES files(id),
    imported_name TEXT NOT NULL,
    module_spec   TEXT,                              -- raw specifier, e.g. './foo', 'foo/bar'
    to_file_id    TEXT REFERENCES files(id),         -- NULL when the spec is external/unresolved
    to_symbol_id  TEXT REFERENCES symbols(id),       -- NULL when only the file is known
    valid_from    TEXT NOT NULL,
    valid_to      TEXT
);

CREATE INDEX IF NOT EXISTS idx_file_imports_repo ON file_imports(repo_id);
CREATE INDEX IF NOT EXISTS idx_file_imports_from ON file_imports(from_file_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_file_imports_uniq
    ON file_imports(from_file_id, imported_name, valid_from);

-- How a symbol edge's target was resolved. Existing rows predate the scoped
-- resolver, so they are backfilled to 'legacy'.
ALTER TABLE symbol_edges ADD COLUMN resolution TEXT NOT NULL DEFAULT 'legacy';

-- How a route's handler symbol was attached, plus a confidence for the match.
ALTER TABLE routes ADD COLUMN resolution TEXT NOT NULL DEFAULT 'legacy';
