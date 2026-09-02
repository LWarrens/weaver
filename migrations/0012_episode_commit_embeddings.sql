-- Add embedding columns to episodes and commits for semantic search coverage.
ALTER TABLE episodes ADD COLUMN embedding BLOB;
ALTER TABLE commits  ADD COLUMN embedding BLOB;
