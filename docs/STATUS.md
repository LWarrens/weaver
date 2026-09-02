# Implementation status

Changelog of what has landed, by phase. Overview and current capabilities are in the [README](../README.md).

## Implementation Phases

### Phase 1 ✅

- Rust workspace and MCP server skeleton
- SQLite migrations and bitemporal schema
- ADR markdown parser (`pulldown-cmark`)
- `sync_adrs_from_git` — parses ADRs, upserts decisions, constraints, and code links
- `find_decisions_for_code` — file-level, symbol-level, and module/service-level resolution (entity mentions + path-segment fallback)

### Phase 2 ✅

- tree-sitter symbol extraction (Rust)
- `ingest_symbols` — walks repo, applies repo-relative glob filters, extracts supported symbols, and persists symbol spans
- `find_decisions_for_code` extended to resolve by symbol name
- `inspect_change_against_decisions` — constraint violation detection over changed file sets
- `query` — keyword search across decisions and ADR titles
- `generate_adr_draft` — sequential-ID draft generation with related decision linking

### Phase 3 ✅

- `record_decision_episode` — episode ingestion from discussions, PR comments, meeting notes; ingest-time embedding
- `get_graph_schema` — concrete schema introspection
- `get_architecture` — high-level ingested-memory summary with community detection results
- Entity resolution for episode-sourced decisions: `entity_nodes` table plus cross-episode decision dedup (normalized-exact or embedding-similarity merge with `supports` edge provenance)
- `temporal_edges` populated for ADR-typed edges (`applies_to`, `depends_on`, `conflicts_with`), Decision→Constraint (`imposes`), and Commit→Decision (`evidences`)
- Vector embedding support: LM Studio, Ollama, OpenAI providers; chunked embedding via `text-splitter`; cosine similarity search over decisions, constraints, episodes, commits, and symbols
 - `embed_all` — backfill embeddings for existing data without embeddings
 - Semantic search end-to-end in `query` via RRF (keyword + semantic + graph)
- LLM provider trait (`src/llm.rs`) used for structured fact extraction in episodes
- Embedding provider trait (`src/embeddings.rs`) with `embed_chunked` default method for chunked embedding
- Call graph extraction via tree-sitter AST edges in `symbol_edges`
- Structural `contains` edges in `symbol_edges` (impl→method, class→method)
- Label propagation community detection stored in `communities` / `community_members`
- HTTP route detection via regex, stored in `routes` table, linked to handler symbols
 - `impact_of` — graph traversal from an ADR across `temporal_edges`
 - `sync_commits_from_git` — git commit ingestion and `decision_git_links` population; ingest-time embedding
 - `adr_lineage` — supersession chain traversal via `supersession_edges`
 - `retract` — soft-delete hallucinated or incorrect LLM-extracted facts; cascade from decision to constraints; optional correction episode
 - `propose_links` — suggest candidate links (decisions, commits, symbols, routes) for an ADR with explicit confidence scores; never auto-promoted
 - `find_stale_decisions` — detect accepted decisions whose linked files or symbols have diverged from implementation reality; five heuristic staleness signals (including `drifted_evidence` from failed anchor verifications)
 - `trace_symbol_history` — reconstruct the full architectural timeline for a named symbol across decisions, constraints, commits, and episodes
 - `trace_call_path` — directional call-chain traversal (inbound/outbound/both) via `symbol_edges`; cycle-safe BFS with configurable depth and confidence threshold
 - `find_orphaned_code` — identify files and symbols with no linked decisions or ADRs; optional path-prefix scope
 - `index_status` — indexing freshness, entity counts, and embedding coverage per entity type
 - `validate_adr` — structural and consistency validation for an ADR file against the ingested knowledge graph
 - `check_consistency` — cross-ADR conflict explanation: explicit conflict edges, contradictory constraints, supersession inconsistencies
 - `diff_architecture` — temporal diff of decisions, constraints, and ADRs between two timestamps or git refs
 - `explain_answer` — full retrieval provenance for a query: keyword/semantic/graph steps and per-decision scores
 - `sync_incremental` — re-index only changed files since a given timestamp or git ref; auto-detects baseline from last ingestion
- Symbol enrichment: `signature`, `return_type`, `visibility`, `is_async`, `complexity`, `decorators` extracted from the same tree-sitter pass
- Content-hash incremental skipping in `ingest_symbols`: unchanged files are skipped; changed files retire old symbols before re-inserting
- Stale symbol retirement: symbols from renamed or deleted files have `valid_to` set rather than being left as ghost records
 - `synthesize_adr_leads` — observe and record undocumented patterns for orphaned files; context injection (co-changed files, semantic decision dedup); `observed_pattern` field; episode-backed provenance
 - `embed_all` extended: entity nodes pass added; all six passes run concurrently with `buffer_unordered(8)`; returns `entity_nodes_embedded`
 - Background jobs: `ingest_symbols` and `synthesize_adr_leads` return a `job_id` and stream progress; polled via `ingest_symbols_status` / `synthesize_adr_leads_status`; `cancel_ingest` stops a running ingest
 - `focused_file_brief` — one-call compact brief (exports, cross-file callers/callees, governing ADRs, recent commits) for a file or symbol
 - `list_repos` / `delete_repo` — repository inventory and irreversible per-repo purge
 - `get_graph_snapshot` — node/edge snapshot for the manager client's graph view
 - `reload_daemon` — hot-reload the Streamable HTTP daemon with no port gap (`--daemon` mode only)

### Phase 4 ✅

Claims, content-hashed evidence anchors, per-evidence freshness, and per-view freshness manifests. See `docs/DESIGN-claims-and-freshness.md`.

- `claims` — decisions and constraints decomposed into individually verifiable assertions, each with an `evidence_grade` (`unknown` / `partial` / `proven`) set at ingest and an obligation `polarity` for constraints
- `evidence_anchors` — content-hashed citation spans (`sha256(normalize_ws(text))` over the span only), canonical identity `(source_kind, source_uri, subpath)`, idempotent insertion; populated by ADR sync (ADR section anchors), episode ingestion (episode-content + resolved-symbol-span anchors), and commit linking (immutable commit anchors)
- `evidence_verifications` — append-only per-anchor checks against the working tree: `edit_class` (`unchanged` / `shifted` / `affected` / `deleted`), `freshness` (`fresh` / `stale`), relocated locator; re-run off the query path on re-ingest
- Three-state claim `disposition`: `unaffected` / `affected` / `unprovable` (terminal)
- `verify_claims` — per-anchor freshness and per-claim disposition for an ADR, decision, file, or symbol
- `freshness` manifests attached to `query`, `find_decisions_for_code`, and `inspect_change_against_decisions` via a `verify` mode; `verify: strict` refuses a stale response with a rebuild obligation (default for `inspect_change_against_decisions`)
- `index_lanes` — per-lane last-indexed commit, commit lag vs HEAD, status, and query capabilities, surfaced by `index_status`
- `verify_index_integrity` — offline ℱ(G) = ℱ(G) oracle: full ADR-lane re-sync vs. live index, run in CI
- `find_stale_decisions` gains a `drifted_evidence` signal; `check_consistency` reads `claims.polarity`; `retract` closes claims on cascade
- migration 0019 drops the never-populated `evidence_spans` table

---
