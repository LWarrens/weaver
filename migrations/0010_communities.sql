CREATE TABLE IF NOT EXISTS communities (
    id          TEXT PRIMARY KEY NOT NULL,
    repo_id     TEXT NOT NULL REFERENCES repositories(id),
    label       TEXT,
    size        INTEGER NOT NULL,
    valid_from  TEXT NOT NULL,
    valid_to    TEXT
);

CREATE TABLE IF NOT EXISTS community_members (
    community_id  TEXT NOT NULL REFERENCES communities(id),
    symbol_id     TEXT NOT NULL REFERENCES symbols(id),
    PRIMARY KEY (community_id, symbol_id)
);
