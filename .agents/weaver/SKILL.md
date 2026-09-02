---
name: weaver
description: Use this skill when working with the weaver repository or when connecting Claude, Codex, Copilot, or another MCP client to its architectural memory tools for ADR ingestion, symbol indexing, decision lookup, change inspection, orphaned code detection, temporal diffs, incremental indexing, retrieval provenance, or ADR draft generation.
---

# Architecture Memory MCP

Use this skill to work on or with `weaver`, a Rust MCP server that stores architectural decisions, ADR evidence, constraints, source symbols, and temporal facts in SQLite.

Treat the server as an architectural control plane, not a chatbot. Prefer structured MCP tool calls over prose guesses, and return evidence-backed results with warnings when support is incomplete.

## Core Rules

- Use SQLite as the runtime source of truth.
- Use ADR markdown as human-facing evidence for accepted decisions.
- Use git history as chronology and evidence, when available.
- Preserve temporal integrity: do not overwrite historical facts destructively.
- Surface conflicts, warnings, uncertainty, and unsupported paths explicitly.
- Do not invent decisions, constraints, links, or compatibility behavior.
- Do not treat discussion as accepted architecture unless the caller provides an explicit accepted decision.

## Local Server

Build and run the MCP server from the repository root:

```sh
cargo build
```

Run it as a persistent Streamable HTTP daemon. Prefer this mode for Claude, Codex, Copilot, and other clients in this repo because stdio subprocess spawning/despawning can make sessions flaky:

```sh
cargo run -- --db ./arch.db --daemon --bind 127.0.0.1:8444
```

To index a repository once before accepting client requests, add `--index-repo`:

```sh
cargo run -- --db ./arch.db --daemon --bind 127.0.0.1:8444 --index-repo C:/Users/me/Projects/personal/weaver
```

Use the release binary for long-lived client configuration:

```sh
target/release/weaver --db /absolute/path/to/arch.db --daemon --bind 127.0.0.1:8444
```

The database file is created automatically. `WEAVER_DB`, `WEAVER_BIND`, `WEAVER_INDEX_REPO`, `WEAVER_INDEX_PATTERN`, and the matching CLI flags can configure daemon startup.

## MCP Client Configuration

Configure clients to connect to the running Streamable HTTP server at:

```text
http://127.0.0.1:8444/mcp
```

Do not configure this repo's normal workflow as stdio unless an MCP client cannot use Streamable HTTP. Stdio can work for one-off smoke tests, but persistent agents should attach to the daemon so tool sessions are not tied to subprocess lifetime.

For VS Code Copilot-style MCP configuration, use the client's Streamable HTTP server form rather than a `type: "stdio"` command. Exact field names vary by client version; the important values are the server name `weaver` and the URL above.

## Tool Workflow

Use tools in this order for normal repository reasoning:

1. Connect to `http://127.0.0.1:8444/mcp`.
2. Call `tools/list` and verify the server tools are present (e.g. `sync_adrs_from_git`, `ingest_symbols`, `query`).
3. If the daemon was not started with `--index-repo`, call `ingest_symbols` before symbol lookup.
4. For repositories with ADRs, call `sync_adrs_from_git`.
5. Use `find_decisions_for_code`, `inspect_change_against_decisions`, or `query` for reasoning.
6. Use `record_decision_episode` for discussions, PR comments, or meeting notes that represent explicit architectural decisions.
7. Use `get_graph_schema` or `get_architecture` for orientation.
8. Use `generate_adr_draft` for ADR drafts and `generate_adr_patch` to produce a non-mutating unified diff.
9. Use `index_status` to check indexing coverage and embedding completeness.
10. Use `sync_incremental` instead of a full re-sync when only a subset of files changed.
11. Use `validate_adr` before syncing a new or modified ADR file.
12. Use `diff_architecture` to compare architectural state between releases or timestamps.
13. Use `explain_answer` to debug why a query returned specific decisions.
14. Use `retract` to soft-delete a hallucinated or incorrect LLM-extracted fact before it propagates.
15. Use `propose_links` to discover candidate relationships between an ADR and commits, symbols, decisions, or routes — all with explicit confidence scores, never auto-promoted.
16. Use `find_stale_decisions` to surface accepted decisions that may no longer align with implementation — deleted files, missing symbols, or no recent commit activity.
17. Use `trace_symbol_history` to get a chronological timeline of how a symbol evolved across commits, decisions, constraints, and episodes.
18. Use `synthesize_adr_leads` to record undocumented patterns for code areas that have no governing ADR. Run `find_orphaned_code` first to identify candidates, then `embed_all` after so leads are searchable.
19. Use `focused_file_brief` for a compact cross-reference of a file or symbol: exports, cross-file callers/callees, governing ADRs, and recent commits — cheaper than chaining `get_architecture` + `trace_call_path` + `find_decisions_for_code`.
20. Use `get_graph_snapshot` to retrieve a node/edge snapshot for interactive graph visualization (consumed by the manager-client UI).
21. Use `reload_daemon` in `--daemon` mode to hot-reload the server without a port gap after binary updates.

## Smoke Test On This Repo

After connecting to the daemon, an MCP client should be able to make this expected tool call:

```json
{
    "method": "tools/call",
    "params": {
      "name": "query",
      "arguments": {
        "repo_path": "C:/Users/me/Projects/personal/weaver",
        "query": "SQLite source of truth",
        "valid_at": null
      }
    }
  }
```

For raw Streamable HTTP smoke tests, send JSON-RPC requests as `POST /mcp` with `Content-Type: application/json` and `Accept: application/json, text/event-stream`. Save the `mcp-session-id` response header from `initialize` and send it on subsequent requests.

This repository currently has no ADRs in the indexed graph, so an empty or low-confidence decision result can be valid. A protocol failure is not valid: `tools/list` must expose the expected tools, and the tool call must return a structured JSON result rather than `method not found`.

## Tool Contracts

### `sync_adrs_from_git`

Use first for any repository with ADR markdown:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "adr_glob": "docs/adr/*.md"
}
```

This parses ADR IDs, titles, status, date, context, decision, consequences, supersession fields, affected code mentions, and constraints. Re-run it after ADR changes.

**ADR detection**: only files whose H1 title or filename carries a numeric ADR identifier (e.g. `ADR-0042`, `0042-title.md`) are ingested. Files that match the glob but carry no ADR identity (README.md, CHANGELOG.md, etc.) are silently skipped.

**Exclusion rules** — two layers prevent broad globs from sweeping unrelated directories:
1. **Gitignore**: paths ignored by the repository's `.gitignore` are automatically excluded (catches `node_modules/`, `dist/`, etc.).
2. **`.archignore`**: place a `.archignore` file in the repository root to add repo-specific exclusions. Each non-comment line is a glob pattern matched against the file's path relative to the repo root (forward slashes, `**` crosses directory boundaries). Example:

```
# .archignore
vendor/**
docs/generated/**
*.min.js
```

**Response shape**: `warnings` is capped at 20 entries. `warnings_total` reports the full count so callers can detect when truncation occurred.

### `ingest_symbols`

Use after ADR sync when symbol-level lookup matters:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "pattern": null
}
```

The default behavior scans all supported source/document formats and skips unsupported or ignored paths. Use `pattern` as a repo-relative glob to narrow indexing.

### `find_decisions_for_code`

Use to answer what decisions govern a file or symbol:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "target": {
    "file": "src/server.rs",
    "symbol": null,
    "module": null,
    "service": null
  },
  "valid_at": null
}
```

Prefer file and symbol targets. Module and service resolution may be incomplete and can return warnings.

### `inspect_change_against_decisions`

Use before or during code changes to check changed files against active constraints:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "files": ["src/server.rs"],
  "change_summary": "Add a Streamable HTTP MCP tool",
  "symbols": [],
  "valid_at": null
}
```

Treat results as possible violations with confidence scores, not final proof.

### `query`

Use for keyword or topic search across stored decisions and constraints:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "query": "ADR supersession",
  "valid_at": null,
  "top_k": 10,
  "graph_depth": 1,
  "min_confidence": 0.0
}
```

Results are ranked by Reciprocal Rank Fusion (RRF) across three retrievers: keyword FTS, semantic vector similarity (requires embeddings), and BFS graph neighbourhood expansion. `graph_depth` is capped at 2. Report returned decisions, constraints, evidence, warnings, temporal context, and confidence.

### `record_decision_episode`

Use for architectural discussions, PR comments, meeting notes, or explicit decision episodes:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "source": "PR review",
  "source_uri": "https://example.invalid/pr/123#discussion",
  "occurred_at": "2026-05-05T00:00:00Z",
  "content": "Discussion text",
  "decisions": [
    {
      "title": "Keep SQLite as MVP source of truth",
      "text": "SQLite remains the MVP source of truth for architectural facts.",
      "constraints": ["Do not add a secondary graph store as a write path."],
      "affected_files": ["src/storage/sqlite.rs"]
    }
  ]
}
```

Episode-backed decisions are not synthetic ADRs. Keep that distinction visible.

### `generate_adr_draft`

Use when a caller asks for an ADR draft:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "title": "Use SQLite for architectural memory",
  "context": "The MVP needs deterministic local persistence.",
  "proposed_decision": "Use SQLite as the source of truth.",
  "affected_files": ["src/storage/sqlite.rs"]
}
```

This returns markdown and an assigned ADR ID. It does not write files.

### `synthesize_adr_leads`

Use to observe and record undocumented architectural patterns for files lacking explicit decisions. Leads are records of what the code *already does* — not proposals. They are retrieved as context when an LLM investigates code with no governing ADR.

The tool pre-filters files with no ingested symbols (pure config/data), injects co-changed files and top-3 semantically related existing decisions into each LLM prompt, and defaults to `min_confidence = 0.5`. By default `dry_run` is false, so leads are persisted as episode-backed provenance.

Sample params:

```json
{
  "repo_path":"/absolute/path/to/repo",
  "path_prefix":"src/",
  "limit":5,
  "min_confidence":0.5,
  "dry_run":false,
  "record_episode":true,
  "episode_source":"synthetic:llm"
}
```

Returns `leads` with `title`, `observed_pattern`, `affected_files`, `confidence`, `markdown`, `patch`, and `episode_id` for provenance. Use `retract` to close any incorrect synthetic episodes.

### `generate_adr_patch`

Use when a caller wants a git-ready ADR patch without writing files:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "adr_path": "docs/adr/ADR-0001-use-sqlite.md",
  "draft": "# ADR-0001 Use SQLite\n\n## Status\n\nProposed\n"
}
```

This returns a unified diff and does not apply it.

### `embed_all`

Backfill vector embeddings for all decisions, constraints, episodes, commits, symbols, and entity nodes that do not yet have one:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "chunk_size": 512
}
```

Requires an embedding provider configured via `WEAVER_EMBEDDING_PROVIDER` (`ollama`, `lmstudio`, or `openai`). Processes all entity types concurrently (up to 8 in-flight requests). Returns counts per entity type (`decisions_embedded`, `constraints_embedded`, `episodes_embedded`, `commits_embedded`, `symbols_embedded`, `entity_nodes_embedded`) and a capped warnings list with `warnings_total`. Run this after ingestion when semantic search via `query` returns no results.

### `sync_commits_from_git`

Ingest git commit history and link commits to decisions by ADR ID reference (confidence 0.95) or keyword overlap (confidence 0.6):

```json
{
  "repo_path": "/absolute/path/to/repo",
  "branch": null,
  "since": "2024-01-01T00:00:00Z",
  "limit": 500
}
```

Commits are deduplicated by SHA so re-running is safe. Embeds commit messages if an embedding provider is configured. Capped warnings with `warnings_total`.

### `adr_lineage`

Return the full supersession lineage for an ADR:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "adr_id": "ADR-0001"
}
```

### `impact_of`

Return decisions, constraints, and symbols that would be impacted by changing a given symbol:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "symbol": "SqliteStore"
}
```

### `trace_call_path`

Trace the call path from one symbol to another across the ingested call graph:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "from_symbol": "run",
  "to_symbol": "insert_decision",
  "max_depth": 6
}
```

### `get_graph_schema`

Use for schema/tool orientation:

```json
{
  "include_counts": true
}
```

### `get_architecture`

Use for a high-level summary of currently ingested memory for a repo:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "valid_at": null
}
```

### `index_status`

Check indexing freshness, entity counts, and embedding coverage:

```json
{
  "repo_path": "/absolute/path/to/repo"
}
```

Returns per-type totals, embedding coverage percentages, and last-ingested timestamps for ADRs, decisions, constraints, episodes, commits, symbols, and files. Use this to decide whether `embed_all` or a re-sync is needed.

### `sync_incremental`

Re-index only files that changed since a git ref or timestamp — faster than a full sync for CI hooks or post-commit runs:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "since": "abc1234",
  "adr_glob": "docs/adr/*.md"
}
```

`since` accepts a commit SHA, tag, branch name, or ISO-8601 timestamp. Omit it to use the most recent ingestion timestamp from the index (indexes everything if the index is empty). Changed `.md` files are re-synced via `sync_adrs_from_git`; changed source files are re-ingested via `ingest_symbols`. Falls back to a full sync only when git is unavailable.

### `validate_adr`

Validate an ADR file for structural completeness and consistency before syncing it:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "adr_path": "docs/adr/0042-use-redis.md"
}
```

`adr_path` may be absolute or relative to `repo_path`. Returns `valid` (bool), `adr_id`, `title`, `status`, `errors` (blocking), and `warnings` (advisory). Errors include: missing H1 title, no ADR ID found, duplicate ADR ID already in the index. Warnings include: missing Status/Context/Decision sections, supersedes/superseded_by IDs not in the index, `superseded` status without a `superseded_by` reference.

### `find_orphaned_code`

Surface files and symbols with no linked ADRs, decisions, or constraints:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "path_prefix": "src/payments"
}
```

`path_prefix` is optional; omit to scan the entire repo. Returns `orphaned_files`, `orphaned_symbols`, `total_files`, `total_symbols`, and `warnings`. Use to identify architectural dark matter — code areas not covered by any decision.

### `diff_architecture`

Compare architectural state between two points in time:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "from": "v1.0.0",
  "to": "v2.0.0"
}
```

`from` and `to` accept ISO-8601 timestamps (`2024-01-15T00:00:00Z`), date strings (`2024-01-15`), commit SHAs, tags, or branch names. `to` defaults to now. Returns `decisions_added`, `decisions_removed`, `constraints_added`, `constraints_removed`, `adrs_added`, `adrs_removed`, and a human-readable `summary`. Uses the bitemporal `valid_from`/`valid_to` fields; no schema changes needed.

### `explain_answer`

Re-run a query with full retrieval provenance — use this to debug why `query` returned specific decisions:

```json
{
  "repo_path": "/absolute/path/to/repo",
  "query": "authentication session tokens",
  "valid_at": null
}
```

Returns `terms_extracted`, `steps` (one per retriever: `keyword_search`, `semantic_search`, `graph_expansion`, `rrf_merge`), `final_decision_ids`, and `warnings`. Each step lists matched decisions with scores and human-readable reasons. `semantic_search` is skipped with a warning if no embedding provider is configured.

### `propose_links`

Suggest candidate links between a target ADR or decision and related commits, symbols, decisions, and routes. All candidates are non-authoritative and carry an explicit confidence score; none are promoted automatically.

```json
{
  "repo_path": "/absolute/path/to/repo",
  "target": "ADR-0042",
  "limit": 5
}
```

`target` accepts an ADR ID (e.g. `"ADR-0042"`) or a decision UUID. `limit` caps candidates per entity type (default 5).

Returns `target_id`, `target_adr_id`, `target_title`, and a `candidates` array. Each candidate has `entity_type` (`"decision"`, `"commit"`, `"symbol"`, `"route"`), `entity_id`, `title`, `confidence` (0.0–1.0), and `reason`. Candidates are sorted by confidence descending. Requires embeddings for semantic matching — run `embed_all` first for best results; falls back to keyword matching without them.

### `find_stale_decisions`

Detect accepted architectural decisions that may no longer align with implementation reality. Runs four heuristic passes:

| Signal | Confidence | Trigger |
|--------|-----------|---------|
| `deleted_file` | 0.85 | Linked file path no longer present in live file index |
| `missing_symbols` | 0.70 | Linked file exists but has no live symbols after a re-ingest |
| `no_recent_activity` | 0.45 | Decision has commit links but none after `since` cutoff |
| `aged_adr` | 0.35 | Accepted ADR older than `since`, not superseded |

```json
{
  "repo_path": "/absolute/path/to/repo",
  "since": "2025-01-01T00:00:00Z",
  "min_confidence": 0.4
}
```

`since` defaults to 180 days ago. Returns `stale_decisions` sorted by confidence descending. Each entry has `decision_id`, `adr_id`, `title`, `valid_from`, `confidence`, and `signals` (list with `kind`, `detail`, `confidence`). Requires prior `ingest_symbols` and `sync_commits_from_git` runs for full signal coverage.

### `trace_symbol_history`

Trace the temporal history of a symbol, file, or route across commits, ADRs, decisions, constraints, and episodes. Returns a timeline sorted oldest → newest.

```json
{
  "repo_path": "/absolute/path/to/repo",
  "symbol": "SqliteStore",
  "valid_from": "2024-01-01T00:00:00Z",
  "valid_to": null
}
```

`symbol` is matched against the ingested `symbols.name` column. `valid_from`/`valid_to` narrow the trace window (both default to unbounded). Returns `symbol`, `current_file`, `first_seen`, `timeline` (array of `TraceEvent`), and `warnings`. Each `TraceEvent` has `occurred_at`, `event_type` (`symbol_seen`, `decision_activated`, `decision_superseded`, `constraint_activated`, `constraint_superseded`, `commit`, `episode`), `entity_id`, `entity_type`, `summary`, and `confidence`. Requires prior `ingest_symbols` run; commit and episode events require `sync_commits_from_git` and `record_decision_episode` respectively.

### `retract`

Soft-delete a hallucinated or incorrect LLM-extracted fact. Idempotent — retracting an already-closed entity adds a warning but does not error.

```json
{
  "repo_path": "/absolute/path/to/repo",
  "entity_type": "decision",
  "entity_id": "uuid-of-the-decision",
  "reason": "LLM hallucinated this decision; no such choice was made",
  "replacement": "Optional correction text stored as an audit episode"
}
```

`entity_type` must be `"decision"`, `"constraint"`, or `"episode"`. Retracting a `"decision"` cascades to all its active constraints. Retracting an `"episode"` closes all decisions and constraints derived from that episode. If `replacement` is provided, a retraction-correction episode is inserted as an audit note and its ID is returned as `replacement_episode_id`.

### `focused_file_brief`

Return a compact LLM-optimised brief for a file or symbol. Use this instead of chaining `get_architecture` + `trace_call_path` + `find_decisions_for_code` separately.

```json
{
  "repo_path": "/absolute/path/to/repo",
  "file": "src/server.rs",
  "symbol": null,
  "max_callers": 5,
  "max_callees": 5,
  "max_commits": 5
}
```

Provide either `file` (repo-relative path) or `symbol` (name); one is required. Returns `file`, `exports` (name, kind, line), `callers` (grouped by file), `callees` (grouped by file), `decisions` (governing ADRs), `commits` (recent), and `warnings`.

### `get_graph_snapshot`

Return a node/edge snapshot for interactive graph visualization (consumed by the manager-client UI):

```json
{
  "repo_path": "/absolute/path/to/repo",
  "root_id": null,
  "depth": 2,
  "limit": 200
}
```

`root_id` is an optional UUID to anchor the subgraph. `depth` controls BFS depth from the root. Returns `nodes` and `edges` arrays suitable for force-directed rendering. Not intended for LLM reasoning — prefer `query` or `impact_of` for decision lookup.

### `reload_daemon`

Hot-reload the daemon: drains in-flight MCP sessions, rebuilds the server, and re-binds the port with no connection gap. Only works when the binary is running in `--daemon` mode:

```json
{}
```

No parameters. Returns a confirmation string. If called outside `--daemon` mode, returns an error message rather than failing the RPC.

## Provider Traits

The server defines two provider traits for optional AI capabilities:

**`EmbeddingProvider`** (`src/embeddings.rs`): `async fn embed(&self, text: &str) -> Result<Vec<f32>>` plus a default `embed_chunked` method that splits long text via `text-splitter` and averages chunk embeddings. Configured via `WEAVER_EMBEDDING_PROVIDER` (`ollama`, `lmstudio`, `openai`). If unset, semantic search falls back to keyword-only.

**`LlmProvider`** (`src/llm.rs`): `async fn generate(&self, prompt: &str) -> Result<String>`. Used by `record_decision_episode` to extract structured facts from episode text. Configured via `WEAVER_LLM_PROVIDER` (`ollama`, `openai`, `mock`). `mock` reads `WEAVER_LLM_RESPONSE` from the environment — useful in tests without HTTP.

Both providers are optional: if unconfigured, the relevant feature degrades gracefully (no embeddings / no fact extraction).

## Repository Development

When modifying this repository:

- Start with MCP graph/tool discovery when available.
- Fall back to file inspection for docs, configs, literal strings, and missing graph coverage.
- Keep changes minimal and tied to an explicit requirement.
- Do not add interfaces, managers, compatibility shims, fallback paths, or generalized adapters unless a current spec requires them and their role is clear.
- Update `README.md` and `TECHNICAL_BEHAVIOR.md` when tool behavior or support status changes.
- Run `cargo test --quiet` before finalizing code changes when feasible.

## Expected Answer Shape

When answering through an MCP client, include:

- the direct answer
- relevant decisions and constraints
- concrete evidence references
- warnings and unsupported areas
- temporal context, especially `valid_at`
- confidence or uncertainty

Never return architecture advice based only on vibes.
