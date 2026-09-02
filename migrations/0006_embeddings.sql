ALTER TABLE decisions     ADD COLUMN embedding BLOB;  -- f32 LE-packed
ALTER TABLE constraints   ADD COLUMN embedding BLOB;
ALTER TABLE adr_documents ADD COLUMN embedding BLOB;
ALTER TABLE symbols       ADD COLUMN embedding BLOB;
