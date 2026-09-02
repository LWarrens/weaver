-- index_lanes: per-repository freshness of each independent index lane, so a
-- caller can see which query capabilities are answerable and how far each lane
-- lags HEAD.
--
--   last_ingested_commit - the commit the lane was last built against
--   status               - ok | failed | absent
--
-- lag_commits (git rev-list --count <commit>..HEAD) is derived at read time,
-- not stored: it is only meaningful relative to current HEAD.

CREATE TABLE IF NOT EXISTS index_lanes (
    repo_id              TEXT NOT NULL REFERENCES repositories(id),
    lane                 TEXT NOT NULL,   -- adr | symbol | commit | embedding | community | route
    last_ingested_commit TEXT,
    last_ingested_at     TEXT NOT NULL,
    status               TEXT NOT NULL,   -- ok | failed | absent
    detail               TEXT,
    PRIMARY KEY (repo_id, lane)
);
