-- Drop the never-populated evidence_spans table (migration 0001).
--
-- It was always schema-only. Phase 4 replaced it with `evidence_anchors` +
-- `evidence_verifications` (migration 0016), which carry content-hashed
-- citations and per-check freshness. Nothing reads or writes evidence_spans.
-- See docs/DESIGN-claims-and-freshness.md, Step 9.

DROP INDEX IF EXISTS idx_evidence_spans_source_id;
DROP TABLE IF EXISTS evidence_spans;
