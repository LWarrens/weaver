# Technical Behavior

This document records the concrete functionality implemented and the current state of each tool and subsystem.

## Implemented

### Infrastructure

- Runtime `sqlx` queries: `sqlx::query` + `.bind()` throughout — no `DATABASE_URL` required at compile time.
- Migrations: `run_migrations()` uses `sqlx::migrate!("./migrations")`; failures surface as `Error::Migration(String)`. Active migrations: 0001 (initial schema), 0002 (episode decision links), 0003 (symbol end line), 0004 (bitemporal effective time), 0005 (symbol edges), 0006 (embeddings — `embedding BLOB` on decisions/constraints/symbols), 0007 (symbol enrichment + `contains` edge type), 0008 (entity nodes), 0009 (contains edge type), 0010 (communities + community_members), 0011 (routes), 0012 (episode and commit embedding columns), 0013 (commit_files), 0014 (file content hash), 0015 (claims), 0016 (evidence anchors + verifications), 0017 (index lanes), 0018 (freshness manifest cache), 0019 (drop unused `evidence_spans`).
- SQLite: WAL mode, `PRAGMA foreign_keys=ON`, `max_connections(1)` (single-writer). All timestamps as ISO-8601 `TEXT`.
- Bitemporal schema: `valid_from`, `valid_to`, `ingested_at`, `source_time` on all meaningful entities.
- SQLite is the only runtime source of truth. There is no secondary embedded graph store or best-effort graph write path.
- MCP server: `src/server.rs` with `rmcp` proc-macro dispatch (`#[tool_router]` + `#[tool_handler]`).
- Daemon transport: Streamable HTTP mounted at `/mcp`; clients should preserve the `mcp-session-id` returned by `initialize`.
- Startup indexing: `--index-repo` runs `ingest_symbols` once after migrations and before serving requests; `--index-pattern` maps to the existing ingest pattern argument.
- Error types: `Storage`, `Migration`, `Parse`, `InvalidInput`, `Other`.

### `sync_adrs_from_git`

- Accepts `repo_path` + `adr_glob`.
- Walks matching files, parses each with `pulldown-cmark`-based ADR parser.
- Extracts: id, title, status, date, context, decision, consequences, supersedes, file/module/service mentions, constraints (via obligation keywords).
- Upserts ADR documents, decisions, constraints, and `decision_code_links` for each file mention.
- Detects supersession: after processing all files, calls `resolve_supersession_edges` which closes ADRs (sets `valid_to`) that are explicitly superseded by a newer ADR in the same repo. The `closed` count in the result refers to this, not to files disappearing from the glob.
- Idempotent: re-syncing unchanged ADRs (same title, status, decision text) increments `unchanged`, not `synced`.
- Per decision and per constraint, inserts a `claims` row (`evidence_grade = proven`) plus a content-hashed `evidence_anchors` row locating the governing ADR section (`Locator::Section`). Constraint claims carry an obligation `polarity` from `detect_polarity`. Records the `adr` index lane as `ok`.

### `find_decisions_for_code`

- Accepts `repo_path`, `target` (file, symbol, module, service), optional `valid_at`.
- **File**: normalizes path lexically (no filesystem touch), validates it doesn't escape the repo root, queries `decision_code_links`. If the file contains entries in the `routes` table, route info is appended to the response (route path, HTTP method, framework, and `handler_id` when the handler symbol is in the same file).
- **Symbol**: queries `find_files_with_symbol`, then resolves decisions for each matching file; deduplicates by decision ID.
- **Module / service**: resolved through `entity_nodes` — case-insensitive name match (typed nodes plus untyped episode-created ones) followed by open `mentions` Decision → Entity edges. Modules additionally fall back to path-segment matching over `decision_code_links` (module "storage" matches decisions linked to files under any `storage/` directory); the answer notes when a match came from the file-path fallback only. An unresolved name yields a warning, never fabricated results.
- Returns `ArchResponse` with decisions, constraints, confidence, warnings, and temporal context.

### `ingest_symbols`

- Accepts `repo_path`, optional repo-relative `pattern` (glob, default `**/*`).
- Skips unsupported extensions and ignored build/dependency folders before reading files.
- **Content-hash skipping**: each file is hashed before parsing. If the stored hash matches and `valid_to IS NULL`, the file is skipped entirely; `files_unchanged` in the response is incremented. The response includes `files_unchanged` alongside the processed file count.
- **Stale symbol retirement**: before inserting fresh symbols for a changed file, all existing symbol records for that file have `valid_to = now` set. This ensures renamed or deleted symbols do not persist as ghost records in the index.
- Extracts symbols through the enabled tree-sitter registry and stores start/end line spans plus enriched metadata: `signature`, `return_type`, `visibility`, `is_async`, `complexity` (cyclomatic, counted from control-flow nodes in the AST), and `decorators` (JSON array). New columns added in migration 0007; all nullable for backward compatibility.
- Upserts file records (with updated content hash) and inserts symbol records with `valid_from` / `ingested_at`.
- The internal run returns an `IngestSymbolsResult` with structured total, processed, unchanged, community, and cancellation fields alongside the existing flattened `ArchResponse`; background job status reads these fields directly rather than parsing the display answer.
- After symbol extraction, runs three additional passes:
  - **Containment pass**: emits `contains` edges in `symbol_edges` for symbol pairs where one symbol's line range strictly spans another's. Edge type `'contains'` is in the `symbol_edges` CHECK constraint (migration 0007). Supported languages: Rust, TypeScript, Python, Java. Graph snapshots surface these as symbol-to-symbol edges and keep file paths as symbol metadata; they do not synthesize file-to-symbol containment for grouping.
  - **HTTP route detection pass**: reuses the eligible-file list from symbol discovery, detects HTTP route definitions via regex, and writes them to the `routes` table (migration 0011), linked to handler symbols when the handler is in the same file. Framework coverage: Express.js (`router.get('/path', handler)`), FastAPI (`@app.get('/path')`), Flask (`@app.route('/path', methods=['GET'])`), Actix-web (`#[get("/path")]`). `handler_id` is set to the handler symbol's ID when resolvable.
  - **Community detection pass**: runs label propagation over CALLS/IMPORTS edges in `symbol_edges`. Clusters symbols into functional modules. Labels are derived from the longest common file path prefix combined with top symbol names; no LLM is required. Results are written to `communities` (migration 0010) and `community_members`.

### `inspect_change_against_decisions`

- Accepts `repo_path`, `files` (changed file paths), optional `change_summary`, optional `symbols`, optional `valid_at`.
- Expands file set via `find_files_with_symbol` for any provided symbol names.
- Loads **all active decisions** for the repo (not just those with explicit file links).
- Fetches all constraints for those decisions.
- Applies keyword-overlap matching: splits each constraint text into words >3 chars, checks if any path segment of each changed file or optional summary term contains that word.
- Confidence scoring: `"high"` if the constraint has extractable words (>3 chars) and the governing ADR status is `"accepted"`; `"medium"` if words matched but the ADR is not accepted; `"low"` if the constraint text yields no extractable words (rare, but possible for very short constraints).
- Deduplicates violations by `(decision_id, constraint_text)`.
- Accepts a `verify` mode (`cached`/`fresh`/`skip`/`strict`), **defaulting to `strict`**. Unless `skip`, builds a freshness manifest over the flagged decisions' claims scoped to the changed-file set and attaches it as `freshness`; in `strict` mode a stale in-scope claim sets `refused` to a rebuild obligation naming the drifted anchors and the sync commands to run.
- Returns `{ possible_violations, relevant_decisions, warnings, temporal_context, freshness?, refused? }`.

### `query`

- Accepts `repo_path`, `query` string, optional `valid_at`, `top_k` (default 10), `graph_depth` (default 1, capped at 2), `min_confidence` (default 0.0).
- Merges three retrieval strategies via **Reciprocal Rank Fusion (RRF)**:
  1. **Keyword** — splits query into words ≥3 chars; OR-matches against `LOWER(d.text) LIKE ?` and ADR/episode titles.
  2. **Semantic** — embeds the query using the configured `WEAVER_EMBEDDING_PROVIDER`, fetches all decisions with stored embeddings, computes cosine similarity, and filters at threshold 0.30. Returns `None` (with a warning) when no provider is configured.
  3. **Graph** — BFS expansion over `temporal_edges` from the top-5 seed decisions up to `graph_depth` hops; fetches neighbor decisions at each hop. Each hop also performs a **commit-bridged expansion**: decisions evidenced by the same commit as a frontier decision (via `evidences` Commit → Decision edges) count as one-hop neighbors.
- RRF score: `Σ 1 / (60 + rank)` across all three lists; ties broken by confidence then title.
- Returns `ArchResponse` with answer string, decisions (up to `top_k`), constraints, confidence.
- Warns when semantic retrieval is skipped or when `graph_depth` was capped.
- Accepts a `verify` mode (`cached`/`fresh`/`skip`/`strict`, default `cached`); unless `skip`, attaches a `freshness` manifest aggregating the returned decisions' claim dispositions, stale/incomplete claims, and index-lane lag. `find_decisions_for_code` takes the same parameter and behaves the same way.

### `record_decision_episode`

- Accepts `repo_path`, `source`, optional `source_uri`, `occurred_at`, `content`, and optional structured `decisions`.
- Inserts an `episodes` row scoped to the repository.
- **Ingest-time embedding**: if `WEAVER_EMBEDDING_PROVIDER` is configured, embeds `content` → `episodes.embedding`, each decision's `text` → `decisions.embedding`, and each constraint text → `constraints.embedding` immediately after insertion.
- Structured fact extraction via `crate::llm::provider_from_env()` (`WEAVER_LLM_PROVIDER`). When no LLM provider is configured, the warning `"no LLM provider configured; facts not extracted"` is added and `facts_extracted` is 0.
- **Cross-episode decision dedup (entity resolution)**: before inserting, each incoming decision is compared against all open decisions for the repo — a normalized-exact text match (lowercased, whitespace-collapsed) merges outright; otherwise, when both sides have embeddings, the highest cosine similarity at or above `dedup_threshold` (default 0.9, clamp 0–1; 1.0 = exact-only) wins. On merge, no new decision row is inserted; a `supports` Episode → Decision edge is emitted (confidence = similarity, evidence ref = episode, idempotent), a warning names the merge target and similarity, and the answer reports the merged count. New constraints and file links are attached to the existing decision; ones it already carries (by normalized text / path) are skipped. Duplicates within one episode's decision list merge the same way.
- Stores each non-duplicate episode decision directly in `decisions` with `episode_id` set and `adr_id` unset.
- Does not create synthetic `adr_documents` rows for episode decisions.
- Per episode decision and constraint, inserts a `claims` row (`evidence_grade = partial` — agent/LLM-supplied, no source offsets) anchored to the whole episode content. Where a mentioned entity resolves to exactly one live symbol, an additional `Locator::SymbolQn` anchor to that symbol's span is added to the decision claim so freshness tracks the code. Merged decisions reuse the surviving decision's claim.
- Stores provided constraints and affected-file links so episode decisions can be found through `query` and `find_decisions_for_code`. Each stored constraint also emits an `imposes` Decision → Constraint edge in `temporal_edges` (evidence ref: the episode), mirroring `sync_adrs_from_git`.
- Returns `ArchResponse` with recorded decision summaries. Episode-backed summaries use `status = "episode"`, `adr_id = "episode:{episode_id}"`, and `episode_id` set to the linked episode UUID.

### `generate_adr_draft`

- Accepts `repo_path`, `title`, optional `context`, `proposed_decision`, `affected_files`.
- Reads `MAX(adr_number)` from the repo's ADR documents to assign the next sequential ID (`ADR-{n:04}`).
- Finds related existing decisions via keyword search on the title (top 5).
- Renders a Markdown template with sections: Status, Date, Context, Decision, Consequences, Constraints, Alternatives Considered, Affected Code, Related Decisions.
- Does not write files. Returns `{ id, markdown, warnings }`.

### `generate_adr_patch`

- Accepts `repo_path`, repo-relative `adr_path`, and complete ADR `draft` Markdown.
- Validates the target path stays inside the repository.
- Reads the existing ADR file if present, then returns a unified diff for create/update.
- Does not write files or apply the patch.

### `get_graph_schema`

- Accepts `include_counts`.
- Returns known SQLite graph tables, table roles, columns, node/edge table groupings, optional row counts, and warnings for schema-only tables.

### `get_architecture`

- Accepts `repo_path`, optional `valid_at`.
- Returns repo-scoped counts, active decision summaries, symbol communities, warnings, and temporal context.
- Communities are returned with size, central symbols, file list, and governing ADRs joined via `decision_code_links`. If community detection has not run (i.e., `ingest_symbols` has not been called), the communities list is empty.
- This is a summary of ingested memory, not a Codebase-Memory-style call graph.

### `impact_of`

- Accepts `repo_path`, `adr_id`, optional `max_depth` (default 3), optional `edge_types` (default: `["applies_to", "depends_on", "conflicts_with"]`), optional `valid_at`.
- Traverses `temporal_edges` from the given ADR outward using a breadth-first walk, respecting `max_depth`.
- Returns affected files, symbols, and decisions for each reachable node, with the hop depth and edge confidence recorded.
- Applies `valid_at` filtering: only edges where `valid_from <= valid_at AND (valid_to IS NULL OR valid_to > valid_at)` are traversed.
- Cycle detection prevents infinite loops in circular edge graphs.

### `sync_commits_from_git`

- Accepts `repo_path`, optional `branch` (defaults to the repo's HEAD branch), optional `since` (ISO 8601), optional `limit`.
- Uses the `git2` crate to walk commit history on the specified branch.
- Upserts each commit into the `commits` table (idempotent by commit SHA).
 - **Ingest-time embedding**: if `WEAVER_EMBEDDING_PROVIDER` is configured, embeds each new commit's message → `commits.embedding` immediately after insertion. Already-stored commits are not re-embedded; use `embed_all` for backfill.
- Creates `decision_git_links` with two confidence tiers:
  - `0.95` for commits where the message contains an explicit ADR ID (e.g., `ADR-0042`).
  - `0.6` for commits where keyword overlap is detected between the commit message and existing decision text.
- Each created link also emits an `evidences` Commit → Decision edge in `temporal_edges` (same confidence as the link), inserted only if no open edge with the same type/source/target exists, so graph traversal can reach decisions through the commits that implement them.
- For the `0.95` explicit-ADR tier, also anchors the decision's open claims to the commit (`AnchorSource::Commit`, `source_uri` = SHA, anchored text = commit message). Commits are immutable so these anchors verify as fresh without filesystem access. Records the `commit` index lane as `ok`.
- Returns `{ commits_ingested, commits_unchanged, links_created, warnings }`.
- Idempotent: re-running for the same commits increments `commits_unchanged` rather than inserting duplicates.

### `embed_all`

- Accepts `repo_path`, optional `chunk_size` (default 512 chars).
- Returns immediately with a single warning if `WEAVER_EMBEDDING_PROVIDER` is not set.
- Fetches all entities without an embedding (decisions, constraints, episodes, commits, symbols, **entity nodes**) using `fetch_*_without_embeddings(repo_id)` storage methods.
- Wraps the embedding provider in `Arc<dyn EmbeddingProvider>` and processes all six passes with `futures::stream::buffer_unordered(8)` — up to 8 concurrent embedding requests per pass.
- Embeds each entity text via `provider.embed_chunked(text, chunk_size)` and writes the packed f32 blob via the corresponding `update_*_embedding` storage method.
- Skips empty texts (commits, symbols, entity nodes). Collects per-item warnings on embedding failure without aborting the run.
- Returns `{ decisions_embedded, constraints_embedded, episodes_embedded, commits_embedded, symbols_embedded, entity_nodes_embedded, warnings, warnings_total }`.
- Idempotent: only items without an existing embedding are processed.
- Requires `futures = "0.3"` in `Cargo.toml`.

---

### `synthesize_adr_leads`

- Accepts `repo_path`, optional `path_prefix`, `limit`, `min_confidence` (default `0.5`), `dry_run`, `record_episode` (default `true`), `episode_source`.
- Runs `find_orphaned_code` to collect files with no governing decisions.
- **Pre-filter**: skips files with no ingested symbols — pure config/data files with nothing architectural to reason about. Increments `skipped` and continues.
- Per surviving file, injects three context blocks into the LLM prompt:
  1. **Co-changed files**: top-5 files most frequently committed alongside the target file (`fetch_cochanged_files`).
  2. **Related existing decisions**: if an embedding provider is configured, embeds `path + symbol names` and fetches the top-3 semantically nearest decisions (`semantic_decisions_if_available`). Falls back to `[]` without embeddings.
  3. **Recent commits**: last 3 commits touching the file.
- Prompt instructs the LLM to describe what the code *already does* (present tense), span ≥ 2 files, and return a single JSON object: `{"title", "observed_pattern", "rationale", "affected_files", "confidence"}`.
- Parses the LLM response via `extract_json_array`, then deserializes as `Vec<serde_json::Value>`, filters for objects only (drops bare strings small LLMs sometimes return), then converts with `serde_json::from_value`. Empty object arrays are silently skipped.
- Accepts both `observed_pattern` and legacy `proposed_decision` JSON keys for backward compat.
- Discards leads below `min_confidence`. Generates an ADR draft and git patch for each kept lead. Records a `synthetic:llm` episode for provenance when `record_episode = true` and `dry_run = false`.
- Episode `entities` is populated from ingested symbol names so entity nodes are linked and embeddable.
- Returns `{ leads: [{ id, title, markdown, patch, affected_files, confidence, episode_id, warnings }], summary: { candidates_examined, synthesized, skipped, warnings } }`.
- LLM provider is required; if unconfigured, all files are skipped and a single warning is emitted.
- Implementation: `src/tools/synthesize_adr_leads.rs`. Storage helpers: `fetch_cochanged_files`, `fetch_symbols_for_file`, `fetch_recent_commits_for_file`.

---

### `adr_lineage`

- Accepts `repo_path`, `adr_id`, optional `max_hops`.
- Queries `supersession_edges` to walk both directions: ancestors (what this ADR superseded) and descendants (what later superseded this ADR).
- Returns `{ root, superseded, superseded_by, current_authority, warnings }`.
  - `root`: the queried ADR ID.
  - `superseded`: ordered list of ADR IDs that this ADR (or its ancestors) superseded.
  - `superseded_by`: ordered list of ADR IDs that supersede this ADR (or its descendants).
  - `current_authority`: the live accepted ADR at the end of the descendant chain, or `null` if the ADR is still current.
  - `warnings`: cycle detection warnings if a loop is encountered.
- No schema changes required; uses the existing `supersession_edges` table populated by `sync_adrs_from_git`.

### `trace_call_path`

- Accepts `repo_path`, `symbol_name`, optional `direction` (`"outbound"` default, `"inbound"`, or `"both"`), optional `max_depth` (default 4), optional `min_confidence` (default 0.5), optional `valid_at`.
- Symbol name lookup: exact match first, then suffix match (e.g. `connect` matches `SqliteStore::connect` when unambiguous).
- BFS over `symbol_edges` filtered by `min_confidence` at traversal time and bounded by `max_depth`.
- `valid_at` filtering: only edges where `valid_from <= valid_at AND (valid_to IS NULL OR valid_to > valid_at)` are traversed.
- Visited-set cycle detection prevents infinite loops on mutual recursion.
- Returns `{ root, chain, truncated, warnings }`. `truncated: true` when `max_depth` was reached and additional nodes exist. `root` is `null` when the symbol is not found in the index.

---

### `find_orphaned_code`

- Accepts `repo_path`, optional `path_prefix` (relative to repo root).
- Queries `files LEFT JOIN decision_code_links` and returns files with zero matching links.
- Queries `symbols LEFT JOIN decision_code_links` (via file) and returns symbols with no governing decisions.
- `path_prefix` is applied as a `LIKE` filter on the file path.
- Returns `{ orphaned_files, orphaned_symbols, total_files, total_symbols, warnings }`. Each orphan includes a `reason` string.
- Requires `ingest_symbols` to have been run; warns when the files table is empty.

---

### `index_status`

- Accepts `repo_path`.
- Runs aggregate queries over existing tables; no new tables required.
- Returns `{ repo_path, index_state, lanes, warnings }`. `index_state` contains:
  - `adrs`: `{ total, last_sync_at }`
  - `files`: `{ total, last_ingested_at }`
  - `decisions`, `constraints`, `episodes`, `commits`, `symbols`, `entity_nodes` (when present): `{ total, embedded, coverage, last_ingested_at }` where `coverage = embedded / total` (1.0 when total is 0).
- `lanes` is one entry per recorded index lane (`adr`, `symbol`, `route`, `community`, `commit`, `embedding`): `{ lane, last_ingested_commit, lag_commits, status, capabilities }`. `lag_commits` is `git rev-list --count <commit>..HEAD`; `capabilities` lists the query modes the lane currently enables.

---

### `validate_adr`

- Accepts `repo_path`, `adr_path` (absolute or relative to repo root).
- Reads and parses the ADR file with the existing `adr_parser`.
- Validation checks: missing title, missing or empty decision text, missing status, unknown status value, duplicate ADR ID in `adr_documents` for the same repo, invalid supersession references (supersedes/superseded_by pointing to non-existent ADRs).
- Does not write to the database.
- Returns `{ valid, adr_id, title, status, errors, warnings }`. `valid` is `true` only when `errors` is empty.

---

### `check_consistency`

- Accepts `repo_path`, optional `valid_at`, optional `min_confidence` (default 0.0).
- Read-only; never auto-resolves. Three detectors populate `ArchResponse.conflicts` as explained candidates with per-conflict confidence:
  1. **Explicit edges** — open `conflicts_with` temporal edges between the repo's decisions, with endpoint labels and recording time.
  2. **Contradictory constraints** — pairs of open constraints from different decisions with opposite obligation polarity sharing ≥ 2 content terms covering at least half of the smaller term set. Polarity is read from the constraint's claim (`claims.polarity`, set at ingest) and falls back to a text scan ("must not", "never", "prohibit", ...) when no claim exists. Confidence 0.5, raised to 0.75 when the two decisions also govern overlapping files (via `decision_code_links`). Pairwise scan is skipped with a warning above 400 constraints.
  3. **Supersession inconsistencies** — a superseded ADR still open (`superseded_but_active`, 0.8) and mutual supersession (`supersession_cycle`, 0.9).
- Results are filtered by `min_confidence` and sorted by confidence descending; the answer string reports per-kind counts.

### `verify_claims`

- Accepts `repo_path` and one of `adr_id`, `decision_id`, `file`, `symbol`; optional `verify` mode.
- Resolves the target to a set of claim ids, verifies each claim's anchors against the current working tree, and returns `{ target, repo_ref, repo_commit, claims, manifest, warnings }`.
- Each claim entry carries its `evidence_grade`, per-anchor `freshness` + `edit_class` (`unchanged`/`shifted`/`affected`/`deleted`), the relocated locator when the span moved, the three-state `disposition` (`unaffected` / `affected` / `unprovable`), and the read-set identities its anchors do not cover.
- Immutable sources (episodes, PRs, commits) verify as fresh without filesystem access. Verifications are cached per resolved HEAD commit; `verify: fresh`/`strict` force re-verification.

### `verify_index_integrity`

- Accepts `repo_path`, optional `adr_glob` (default `docs/adr/*.md`).
- Rebuilds the ADR lane in a throwaway in-memory store via a full `sync_adrs_from_git`, then diffs the declared-fact projection (per claim: kind, polarity, normalized text, sorted anchor content-hashes) against the live index.
- Returns `{ repo_path, consistent, lanes, warnings }`. The `adr` lane is `clean` or `divergent` (with `only_in_live` / `only_in_rebuild` projection keys); `symbol` and `commit` lanes are reported `not_audited`.
- Off the query hot path; exercised by `tests/index_integrity.rs` and a dedicated CI step.

### `diff_architecture`

- Accepts `repo_path`, `from` (ISO-8601 or git ref), optional `to` (ISO-8601 or git ref; defaults to now).
- Resolves git refs to commit timestamps using the `git2` crate; ISO-8601 strings are used directly.
- Queries `decisions`, `constraints`, and `adr_documents` for rows whose `valid_from` falls in `[from_ts, to_ts)` (added) or whose `valid_to` falls in `[from_ts, to_ts)` (removed).
- Returns `{ from_timestamp, to_timestamp, decisions_added, decisions_removed, constraints_added, constraints_removed, adrs_added, adrs_removed, summary }`. Summary is a human-readable one-liner.

---

### `explain_answer`

- Accepts `repo_path`, `query`, optional `valid_at`.
- Re-runs the same retrieval pipeline as `query` but captures per-step provenance.
- Three steps recorded: `keyword_search` (terms and LIKE hits), `semantic_search` (cosine scores per decision; skipped with a warning when no embedding provider), `graph_expansion` (BFS hops and neighbor decision IDs), `rrf_merge` (final RRF scores).
- Returns `{ query, terms_extracted, steps, final_decision_ids, warnings }`. Each step includes `matched` entries with `decision_id`, `title`, `adr_id`, `score`, and `reason`.
- Does not write to the database.

---

### `sync_incremental`

- Accepts `repo_path`, optional `since` (ISO-8601 or git ref), optional `adr_glob` (default `"docs/adr/*.md"`).
- Resolves `since`: if provided as a git ref, converts to a commit timestamp via `git2`; if omitted, uses the most recent `ingested_at` across decisions, symbols, ADRs, and commits; if the index is empty, uses epoch (indexes everything).
- Identifies changed files by listing git commits since `since_resolved` and collecting their modified paths.
- ADR files matching `adr_glob` in the changed set are re-synced via `sync_adrs_from_git`.
- Source files in the changed set are re-indexed via `ingest_symbols` (with per-file content-hash skipping still applied).
- Returns `{ since_resolved, changed_files, adrs_resynced, sources_reindexed, warnings }`.

---

## Test Coverage

Tests cover the implemented tool and storage behavior:

| Module | Tests |
|---|---|
| `domain::adr_parser` | 4 (ADR parsing edge cases) |
| `adapters::registry` | 1 (non-Rust extractors fail instead of returning sentinel symbols) |
| `tools::sync_adrs` | 7 (create, idempotent, history preservation, change detection, file/symbol lookup) |
| `tools::sync_commits` | 1 (ingest + idempotency over real git repo) |
| `tools::ingest_symbols` | symbol persistence, stale symbol retirement, pattern filtering, ignored binary/build artifacts |
| `tools::inspect_change` | 3 (accepted/proposed confidence, no violation for unrelated file) |
| `tools::architecture_query` | 6 (match found, no match, cosine search, temporal edge expansion, top_k cap, two-hop graph) |
| `tools::generate_adr_draft` | 2 (next ID + all sections, empty store gets ADR-0001) |
| `tools::generate_adr_patch` | create patch generation, path containment |
| `tools::get_graph_schema` | schema tables and counts |
| `tools::get_architecture` | empty repo summary warnings |
| `storage::sqlite` | episode decision migration columns, symbol span migration/storage, direct episode decision storage |
| `tools::record_episode` | 2 (no synthetic ADR rows, traceable episode decision; mock LLM fact extraction) |
| `embeddings` | pack/unpack roundtrip, cosine similarity (identical, orthogonal, zero), provider_from_env returns None, embed_chunked averages chunks |

All pass: `cargo test --quiet` (134 at time of writing: 130 unit tests, 2 doc tests, plus two integration tests — `tests/fixture_repo.rs` (builds a real git repository with ADRs, source files, and ADR-referencing commits, runs sync_adrs → ingest_symbols → sync_commits, and asserts on decisions, constraints, defines/imposes/evidences edges, idempotent re-runs, commit-bridged graph expansion, and an end-to-end query) and `tests/index_integrity.rs` (multi-sync ADR lane vs. full re-sync))

---

## Tracked Paper-Alignment Gaps

These are known gaps against the Codebase-Memory and Zep/Graphiti directions. They are tracked explicitly so the current implementation does not imply support it does not have:

- Entity resolution for episode-sourced decisions: `entity_nodes` is populated, and `record_decision_episode` deduplicates semantically similar decisions across (and within) episodes, merging with `supports`-edge provenance. Remaining gap: no retroactive merge of duplicates ingested before dedup existed (a backfill would need to re-embed and re-link).
- General graph traversal: `impact_of` traverses `temporal_edges` for impact/reachability from a given ADR; `query` fuses keyword, semantic, and graph retrieval via RRF, including commit-bridged neighbor expansion. `temporal_edges` is populated for ADR-typed edges, Decision→Constraint (`imposes`, from both ADR sync and episode ingestion), and Commit→Decision (`evidences`, from `sync_commits_from_git`).
- Git evidence graph: `commits` and `decision_git_links` are populated by `sync_commits_from_git`. `pull_requests` table exists but is not yet populated by a tool.
- `evidence_spans`: dropped in migration 0019. Superseded by the Phase 4
  claims / evidence-anchor model (`docs/DESIGN-claims-and-freshness.md`):
  `claims`, `evidence_anchors`, `evidence_verifications`, `index_lanes`, and
  `freshness_manifests` (migrations 0015–0019). ADR sync, episode ingestion, and
  commit linking populate them; `verify_claims`, `verify_index_integrity`, and
  the `freshness` manifests on `query` / `find_decisions_for_code` /
  `inspect_change_against_decisions` read them.

---

## Provider Traits

### `EmbeddingProvider` (`src/embeddings.rs`)

Trait with two methods:
- `async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>` — embed a single text; override point for each backend.
- `async fn embed_chunked(&self, text: &str, max_chars: usize) -> anyhow::Result<Vec<f32>>` — **default method**: splits text with `text-splitter`, embeds each chunk, returns component-wise average. Providers may override for native batching.

Concrete implementations: `LmStudioEmbeddingProvider`, `OllamaEmbeddingProvider`, `OpenAIEmbeddingProvider`.

Factory: `provider_from_env() -> Option<Box<dyn EmbeddingProvider>>` reads `WEAVER_EMBEDDING_PROVIDER`.

Utilities: `pack_f32` / `unpack_f32` (little-endian f32 blob), `cosine_similarity`.

### `LlmProvider` (`src/llm.rs`)

Trait with one method:
- `async fn generate(&self, prompt: &str) -> anyhow::Result<String>` — send a prompt and return the model's text response.

Concrete implementations: `OllamaLlmProvider`, `OpenAILlmProvider`, `MockLlmProvider`.

Factory: `provider_from_env() -> Option<Box<dyn LlmProvider>>` reads `WEAVER_LLM_PROVIDER`.

`MockLlmProvider` returns the value of `WEAVER_LLM_RESPONSE` as-is — used in tests without an HTTP call.

---

## How to Run

Build:

```sh
cargo build
```

Run with PowerShell environment variables (Windows):

```powershell
$env:WEAVER_EMBEDDING_PROVIDER = "lmstudio"
$env:WEAVER_EMBEDDING_URL      = "http://localhost:1234"
$env:WEAVER_EMBEDDING_MODEL    = "text-embedding-harrier-270m"
$env:WEAVER_LLM_PROVIDER       = "lmstudio"
$env:WEAVER_LLM_URL            = "http://localhost:1234"
$env:WEAVER_LLM_MODEL          = "bonsai-1.7b"
$env:WEAVER_LLM_API_KEY        = "lm-studio"

cargo run -- --db .\arch.db
```

Or pass configuration as CLI flags (cross-platform):

```sh
cargo run -- --db ./arch.db \
  --embedding-provider lmstudio \
  --embedding-url http://localhost:1234 \
  --embedding-model text-embedding-harrier-270m \
  --llm-provider lmstudio \
  --llm-url http://localhost:1234 \
  --llm-model bonsai-1.7b \
  --llm-api-key lm-studio
```

No `DATABASE_URL` environment variable required.

---

## Next Steps

- Code retrieval tools (`get_code_snippet`, `search_code`) with path containment.
- `pull_requests` table population via a git/GitHub ingestion tool.

