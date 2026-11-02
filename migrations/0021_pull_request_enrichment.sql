-- ---------------------------------------------------------------------------
-- Pull request ingestion (REQ-032)
--
-- The 0001 `pull_requests` table was scaffolded but never populated. It lacks
-- (a) a uniqueness constraint, so `INSERT OR IGNORE` idempotency is impossible,
-- (b) a merge sha, so PR->commit association is impossible, and (c) any way to
-- distinguish a git-derived stub from a GitHub-API-enriched record.
-- ---------------------------------------------------------------------------

ALTER TABLE pull_requests ADD COLUMN pr_number  INTEGER;                    -- NULL when only a branch name is known
ALTER TABLE pull_requests ADD COLUMN merge_sha  TEXT;                       -- sha of the merge/squash commit; the git join key
ALTER TABLE pull_requests ADD COLUMN author     TEXT;
ALTER TABLE pull_requests ADD COLUMN state      TEXT;                       -- merged|open|closed ('merged' when derived from git)
ALTER TABLE pull_requests ADD COLUMN url        TEXT;
ALTER TABLE pull_requests ADD COLUMN enrichment TEXT NOT NULL DEFAULT 'git'; -- git|github
ALTER TABLE pull_requests ADD COLUMN embedding  BLOB;

-- `pr_ref` is the canonical dedup key: the PR number as text when parseable,
-- otherwise the source branch name.
CREATE UNIQUE INDEX IF NOT EXISTS idx_pull_requests_repo_ref  ON pull_requests(repo_id, pr_ref);
CREATE INDEX        IF NOT EXISTS idx_pull_requests_merge_sha ON pull_requests(merge_sha);

-- ---------------------------------------------------------------------------
-- decision_git_links deduplication
--
-- `insert_decision_git_link` has always used INSERT OR IGNORE, but the table
-- has no unique constraint, so re-running sync_commits_from_git after adding a
-- decision silently accumulates duplicate links. PR linking reuses the same
-- path, so fix it here: collapse existing duplicates, then add the constraint
-- the INSERT OR IGNORE always assumed.
-- ---------------------------------------------------------------------------

DELETE FROM decision_git_links
WHERE id NOT IN (
    SELECT MIN(id)
    FROM decision_git_links
    GROUP BY decision_id, ref_type, ref_id, link_source
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_dgl_unique
    ON decision_git_links(decision_id, ref_type, ref_id, link_source);
