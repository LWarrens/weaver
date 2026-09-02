PRAGMA foreign_keys = OFF;

ALTER TABLE episodes ADD COLUMN repo_id TEXT REFERENCES repositories(id);
CREATE INDEX IF NOT EXISTS idx_episodes_repo_id ON episodes(repo_id);

UPDATE episodes
SET repo_id = (
    SELECT a.repo_id
    FROM adr_documents a
    WHERE a.source_uri = 'episode:' || episodes.id
    LIMIT 1
)
WHERE repo_id IS NULL;

CREATE TABLE decisions_new (
    id            TEXT PRIMARY KEY NOT NULL,
    title         TEXT,
    adr_id        TEXT REFERENCES adr_documents(id),
    episode_id    TEXT REFERENCES episodes(id),
    text          TEXT NOT NULL,
    source_uri    TEXT NOT NULL,
    valid_from    TEXT NOT NULL,
    valid_to      TEXT,
    ingested_at   TEXT NOT NULL,
    source_time   TEXT,
    confidence    REAL NOT NULL DEFAULT 1.0,
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    CHECK ((adr_id IS NOT NULL) <> (episode_id IS NOT NULL))
);

INSERT INTO decisions_new
    (id, title, adr_id, episode_id, text, source_uri, valid_from, valid_to,
     ingested_at, source_time, confidence, evidence_refs)
SELECT d.id,
       CASE
           WHEN a.adr_id LIKE 'EPISODE-%' AND a.source_uri LIKE 'episode:%' THEN a.title
           ELSE NULL
       END,
       CASE
           WHEN a.adr_id LIKE 'EPISODE-%' AND a.source_uri LIKE 'episode:%' THEN NULL
           ELSE d.adr_id
       END,
       CASE
           WHEN a.adr_id LIKE 'EPISODE-%' AND a.source_uri LIKE 'episode:%' THEN substr(a.source_uri, 9)
           ELSE NULL
       END,
       d.text, d.source_uri, d.valid_from, d.valid_to,
       d.ingested_at, d.source_time, d.confidence, d.evidence_refs
FROM decisions d
JOIN adr_documents a ON a.id = d.adr_id;

DROP TABLE decisions;
ALTER TABLE decisions_new RENAME TO decisions;

CREATE INDEX IF NOT EXISTS idx_decisions_adr_id ON decisions(adr_id);
CREATE INDEX IF NOT EXISTS idx_decisions_episode_id ON decisions(episode_id);
CREATE INDEX IF NOT EXISTS idx_decisions_valid_to ON decisions(valid_to);

DELETE FROM adr_documents
WHERE adr_id LIKE 'EPISODE-%'
  AND source_uri LIKE 'episode:%';

PRAGMA foreign_keys = ON;
