CREATE TABLE IF NOT EXISTS routes (
    id          TEXT PRIMARY KEY NOT NULL,
    repo_id     TEXT NOT NULL REFERENCES repositories(id),
    method      TEXT,
    path        TEXT NOT NULL,
    framework   TEXT,
    handler_id  TEXT REFERENCES symbols(id),
    file_path   TEXT NOT NULL,
    line        INTEGER NOT NULL,
    confidence  REAL NOT NULL DEFAULT 1.0,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT
);

CREATE INDEX IF NOT EXISTS idx_routes_repo ON routes(repo_id);
CREATE INDEX IF NOT EXISTS idx_routes_file ON routes(file_path);
