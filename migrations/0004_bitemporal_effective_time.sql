ALTER TABLE adr_documents ADD COLUMN effective_from TEXT;
ALTER TABLE adr_documents ADD COLUMN effective_to TEXT;

UPDATE adr_documents
SET effective_from = COALESCE(source_time, date, valid_from)
WHERE effective_from IS NULL;

ALTER TABLE temporal_edges ADD COLUMN effective_from TEXT;
ALTER TABLE temporal_edges ADD COLUMN effective_to TEXT;

UPDATE temporal_edges
SET effective_from = valid_from
WHERE effective_from IS NULL;

CREATE INDEX IF NOT EXISTS idx_adr_documents_effective_to ON adr_documents(effective_to);
CREATE INDEX IF NOT EXISTS idx_te_effective_to ON temporal_edges(effective_to);
