-- Track the SHA-256 hash of each file's content so incremental re-indexing
-- can skip files that have not changed since the last ingest run.
ALTER TABLE files ADD COLUMN content_hash TEXT;
