ALTER TABLE symbols ADD COLUMN signature   TEXT;
ALTER TABLE symbols ADD COLUMN return_type TEXT;
ALTER TABLE symbols ADD COLUMN visibility  TEXT;
ALTER TABLE symbols ADD COLUMN is_async    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE symbols ADD COLUMN complexity  INTEGER;
ALTER TABLE symbols ADD COLUMN decorators  TEXT;
