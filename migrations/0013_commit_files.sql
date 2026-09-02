-- Create table to map commits to file paths changed in that commit.
CREATE TABLE IF NOT EXISTS commit_files (
    id          TEXT PRIMARY KEY NOT NULL,
    commit_id   TEXT NOT NULL REFERENCES commits(id),
    file_path   TEXT NOT NULL,
    ingested_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_commit_files_commit_id ON commit_files(commit_id);
CREATE INDEX IF NOT EXISTS idx_commit_files_file_path ON commit_files(file_path);
