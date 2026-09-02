# weaver tools

Full request/response contracts for every MCP tool weaver exposes. All tools return strict JSON — no prose-only answers. See the [README](../README.md) for the overview and quickstart.

## Exposed Tools

All tools return strict JSON. No loose prose-only answers.

### Common Response Shape

Most tools return a superset of this shape (extra tool-specific keys are added per tool):

```json
{
  "answer": "string",
  "entities": [],
  "decisions": [],
  "constraints": [],
  "evidence": [],
  "warnings": [],
  "conflicts": [],
  "temporal_context": {
    "valid_at": "string | null",
    "ingested_at": "string | null"
  },
  "confidence": 0.0
}
```

Retrieval tools (`query`, `find_decisions_for_code`, `inspect_change_against_decisions`) accept a `verify` mode (`cached` / `fresh` / `skip` / `strict`) and, unless `skip`, add a per-view `freshness` manifest:

```json
{
  "freshness": {
    "repo_commit": "string",
    "by_disposition": { "unaffected": 0, "affected": 0, "unprovable": 0 },
    "stale_claims": [],
    "incomplete_claims": [],
    "lanes": [],
    "warnings": []
  },
  "refused": null
}
```

Every claim is graded on two independent axes: `evidence_grade` (`unknown` / `partial` / `proven`, set once at ingest — model output never enters at `proven`) and `freshness` (`fresh` / `stale`, recomputed against the working tree). A claim's `disposition` is `unaffected` (all anchors fresh), `affected` (all stale anchors relocated), or `unprovable` (a stale anchor could not be relocated, or the claim has no anchors — terminal, not "low confidence"). Under `verify: strict`, a stale in-scope claim populates `refused` with a rebuild obligation instead of returning a normal answer.

---

### `sync_adrs_from_git`

Scans ADR markdown files from a repository, parses structured fields, inserts/updates records, creates decision-code links, and detects supersession links.

```json
{
  "repo_path": "string",
  "adr_glob": "string | null"
}
```

Parsed from each ADR: id, title, status, date, context, decision, consequences, supersedes, superseded_by, related files/modules/services, constraints. Missing fields are preserved as uncertain — not invented.

---

### `ingest_symbols`

Walks a repository's source files using tree-sitter, extracts symbols (functions, structs, enums, traits, impls, modules, consts, types), and persists start/end line spans plus enriched metadata to the symbol index. Enriched fields per symbol: `signature`, `return_type`, `visibility`, `is_async`, `complexity` (cyclomatic), and `decorators`. Also emits `symbol_edges` (CALLS, IMPORTS, contains), detects communities, and extracts HTTP routes. Required before symbol-level decision lookup works.

Runs in the background: returns a `job_id` immediately and streams progress as MCP log notifications. Content-hash incremental skipping means unchanged files are not re-parsed; pass `force: true` to re-index everything.

```json
{
  "repo_path": "string",
  "pattern": "string | null",
  "force": "boolean | null"
}
```

---

### `ingest_symbols_status`

Checks whether a background `ingest_symbols` job has finished. Returns `job_id`, `status` (`running` | `done` | `failed` | `cancelled`), and `total` / `processed` / `skipped` / `communities` counts.

```json
{ "job_id": "string" }
```

---

### `cancel_ingest`

Requests cancellation of a running `ingest_symbols` job. The job stops cleanly after the current file finishes.

```json
{ "job_id": "string" }
```

---

### `inspect_change_against_decisions`

Inspects a set of changed files and symbols against active architectural decisions and surfaces possible constraint violations with confidence scores. Accepts an optional `verify` mode, **defaulting to `strict`**: when a flagged decision's evidence has drifted, the response carries a `refused` rebuild obligation naming the stale anchors and the sync commands to run.

Violation shape:

```json
{
  "decision_id": "string",
  "adr_id": "string",
  "constraint": "string",
  "matched_files": ["string"],
  "matched_summary": true,
  "confidence": "high | medium | low"
}
```

---

### `record_decision_episode`

Stores an architectural event, extracts candidate entities and relationships. Inferred relationships are marked as low/medium confidence. Discussion is never silently promoted to accepted decision.

Episode decisions are stored as decisions with a direct `episode_id` link. They do not create synthetic `adr_documents` rows.

Incoming decisions are deduplicated against existing open decisions: a normalized-exact text match, or cosine similarity at or above `dedup_threshold` (default 0.9) when embeddings are available, merges the decision into the existing one instead of inserting a duplicate. The merging episode is linked through a `supports` Episode→Decision edge carrying the match similarity; new constraints and file links are attached to the existing decision, already-present ones are skipped.

```json
{
  "repo_path": "string",
  "source": "string",
  "source_uri": "string | null",
  "occurred_at": "string",
  "content": "string",
  "decisions": [
    {
      "title": "string | null",
      "text": "string",
      "constraints": ["string"],
      "affected_files": ["string"]
    }
  ]
}
```

---

### `query`

Searches decisions and constraints using three retrieval strategies merged with Reciprocal Rank Fusion (RRF):

1. **Keyword** — terms (≥3 chars) matched against decision text and ADR titles.
2. **Semantic** — cosine similarity over stored embeddings (requires `WEAVER_EMBEDDING_PROVIDER`). Skipped gracefully when no provider is configured.
3. **Graph** — BFS expansion over `temporal_edges` from the top seed decisions up to `graph_depth` hops, including commit-bridged neighbors (decisions evidenced by a shared commit).

```json
{
  "repo_path": "string",
  "query": "string",
  "valid_at": "string | null",
  "top_k": "integer | null",
  "min_confidence": "number | null"
}
```

---

### `find_decisions_for_code`

Answers what architectural decisions and constraints govern a file, symbol, module, or service. Provide exactly one target key.

```json
{
  "repo_path": "string",
  "target": {
    "file": "string | null",
    "symbol": "string | null",
    "module": "string | null",
    "service": "string | null"
  },
  "valid_at": "string | null"
}
```

* **file** — path is normalized lexically (no filesystem touch) and validated against the repo root, then resolved through `decision_code_links`. Route info is appended when the file has `routes` entries.
* **symbol** — resolves every file that defines the symbol, then unions their decisions.
* **module / service** — resolved through `entity_nodes` name matching plus open `mentions` edges; modules also fall back to path-segment matching, and the response notes when a match was fallback-only.

An unresolved target yields a warning, never fabricated results. Accepts a `verify` mode (see the freshness manifest in the README).

---

### `generate_adr_draft`

Produces an ADR Markdown draft with the next sequential ID auto-assigned from the repository's existing ADRs. Finds related existing decisions from the title via keyword search. Does not write files.

```json
{
  "repo_path": "string",
  "title": "string",
  "context": "string | null",
  "proposed_decision": "string | null",
  "affected_files": ["string"]
}
```

Returns `{ "id": "ADR-NNNN", "markdown": "string", "warnings": [] }`.

---

### `generate_adr_patch`

Produces a git-ready unified diff for a new or updated ADR. It does not write files or apply the patch.

```json
{
  "repo_path": "string",
  "adr_path": "string",
  "draft": "string"
}
```

---

### `get_graph_schema`

Returns the concrete SQLite-backed graph schema: table roles, columns, and (optionally) row counts.

```json
{
  "include_counts": true
}
```

---

### `get_architecture`

Returns a high-level summary of currently ingested architectural memory for a repository: counts, active decisions, detected symbol communities, warnings, and temporal context.

```json
{
  "valid_at": "string | null"
}
```

Communities are returned with size, central symbols, file list, and governing ADRs joined via `decision_code_links`. If `ingest_symbols` has not been run, or if no community detection pass has completed, the communities list will be empty.

---

### `impact_of`

Graph traversal tool. Given an ADR ID, walks typed edges in `temporal_edges` (`applies_to`, `depends_on`, `conflicts_with`) up to `max_depth` hops and returns affected files, symbols, and decisions with depth and confidence at each hop. Cycle detection is included.

```json
{
  "repo_path": "string",
  "adr_id": "string",
  "max_depth": 3,
  "edge_types": ["applies_to", "depends_on", "conflicts_with"],
  "valid_at": "string | null"
}
```

`edge_types` defaults to all three types. `valid_at` filters edges to those active at the given ISO 8601 timestamp.

---

### `sync_commits_from_git`

Ingests git commit history into the `commits` table and creates `decision_git_links`. Uses the `git2` crate to walk commits on a given branch.

Two confidence tiers for linking commits to decisions:
- `0.95` — explicit ADR ID found in the commit message
- `0.6` — keyword overlap between commit message and decision text

```json
{
  "repo_path": "string",
  "branch": "string | null",
  "since": "string | null",
  "limit": "integer | null"
}
```

`since` is an ISO 8601 timestamp. Returns `{ commits_ingested, commits_unchanged, links_created, warnings }`. Idempotent: re-running with the same commits increments `commits_unchanged`.

---

### `embed_all`

Backfills vector embeddings for all decisions, constraints, episodes, commits, symbols, and entity nodes that do not yet have one. Run this after enabling an embedding provider on a repository that was already partially ingested, and again after `synthesize_adr_leads` to make leads searchable.

```json
{
  "repo_path": "string",
  "chunk_size": "integer | null"
}
```

Processes all six entity types with up to 8 concurrent embedding requests. Returns `{ decisions_embedded, constraints_embedded, episodes_embedded, commits_embedded, symbols_embedded, entity_nodes_embedded, warnings, warnings_total }`. Requires `WEAVER_EMBEDDING_PROVIDER` to be configured; returns an explanatory warning and does nothing if no provider is set.

---

### `synthesize_adr_leads`

Observe and record undocumented architectural patterns for files that have no governing ADR. Leads are descriptions of what the code *already does* — not proposals. They are persisted as episode-backed facts and retrieved as context when an LLM investigates code with no formal ADR.

The tool pre-filters files with no ingested symbols, injects co-changed files and top-3 related existing decisions into each LLM prompt to avoid duplication, and defaults to `min_confidence = 0.5`.

Runs in the background: returns a `job_id` immediately. Poll `synthesize_adr_leads_status` with that `job_id` for the result payload below.

```json
{
  "repo_path": "string",
  "path_prefix": "string | null",
  "limit": "integer | null",
  "min_confidence": "number | null",
  "dry_run": "boolean | null",
  "record_episode": "boolean",
  "episode_source": "string"
}
```

Returns:

```json
{
  "leads": [
    {
      "id": "ADR-0042",
      "title": "string",
      "observed_pattern": "string",
      "affected_files": ["string"],
      "confidence": 0.75,
      "markdown": "string",
      "patch": "string | null",
      "episode_id": "uuid | null",
      "warnings": []
    }
  ],
  "summary": {
    "candidates_examined": 10,
    "synthesized": 3,
    "skipped": 7,
    "warnings": []
  }
}
```

Run `embed_all` after to make the new leads searchable via semantic queries. Use `retract` to remove incorrect leads.

---

### `synthesize_adr_leads_status`

Polls a background `synthesize_adr_leads` job. Returns `status` (`running` | `done` | `failed`) and, when done, the full `leads` / `summary` payload.

```json
{ "job_id": "string" }
```

---

### `check_consistency`

Cross-ADR consistency check and conflict explanation. Reports explained conflict candidates — explicit `conflicts_with` edges, contradictory constraints (opposite obligation polarity over shared terms, boosted when the decisions govern overlapping files), and supersession inconsistencies (superseded-but-active ADRs, supersession cycles) — each with its own confidence. Read-only; nothing is auto-resolved.

```json
{
  "repo_path": "string",
  "valid_at": "string | null",
  "min_confidence": 0.0
}
```

---

### `verify_claims`

Verifies the evidence anchors behind an ADR, decision, file, or symbol against the current working tree. Returns, per claim, its `evidence_grade`, each anchor's `freshness` and `edit_class` (`unchanged` / `shifted` / `affected` / `deleted`), any relocated locator, the three-state `disposition`, and the read-set identities its anchors do not cover. Debugging counterpart to the inline `freshness` manifest on retrieval responses.

```json
{
  "repo_path": "string",
  "adr_id": "string | null",
  "decision_id": "string | null",
  "file": "string | null",
  "symbol": "string | null",
  "verify": "cached | fresh | skip | strict | null"
}
```

---

### `verify_index_integrity`

Offline integrity oracle. Rebuilds the ADR index lane from scratch in a throwaway store and diffs its declared-fact projection against the live index, reporting `clean` / `divergent` / `not_audited` per lane plus the diverging claim projections. For CI or manual auditing — never on the query path.

```json
{
  "repo_path": "string",
  "adr_glob": "string | null"
}
```

---

### `adr_lineage`

Traverses the ADR supersession graph via `supersession_edges`. Given an ADR ID, returns the full ancestor chain (what it superseded) and the descendant chain (what superseded it). Cycle detection is included.

```json
{
  "repo_path": "string",
  "adr_id": "string",
  "max_hops": "integer | null"
}
```

Returns:

```json
{
  "root": "string",
  "superseded": ["string"],
  "superseded_by": ["string"],
  "warnings": ["string"]
}
```

`current_authority` is the live accepted ADR at the end of the chain. No schema changes are required; this tool uses the existing `supersession_edges` table.

### `propose_links`

Suggest candidate links between a target ADR or decision and related commits, symbols, decisions, and routes. All candidates are non-authoritative with explicit confidence scores — none are promoted automatically.

```json
{
  "repo_path": "string",
  "target": "ADR-0042",
  "limit": 5
}
```

`target` accepts an ADR ID (e.g. `"ADR-0042"`) or a decision UUID. `limit` caps candidates per entity type (default 5). Candidate sources:

* **Decisions** — keyword overlap + embedding cosine similarity (if embeddings exist)
* **Commits** — commit message keyword overlap + embedding cosine similarity
* **Symbols** — symbols in files explicitly mentioned in the ADR
* **Routes** — routes in explicitly mentioned files, boosted when the route path appears in the ADR text

Returns:

```json
{
  "target_id": "uuid",
  "target_adr_id": "ADR-0042",
  "target_title": "string",
  "candidates": [
    {
      "entity_type": "commit",
      "entity_id": "uuid",
      "title": "commit message summary (sha8)",
      "confidence": 0.74,
      "reason": "commit message keyword overlap + embedding similarity"
    }
  ],
  "warnings": []
}
```

Requires embeddings for semantic matching — run `embed_all` first for best results; falls back to keyword matching when embeddings are absent.

### `find_stale_decisions`

Detect accepted architectural decisions that may no longer align with implementation reality. Runs four heuristic signals and returns decisions sorted by confidence descending.

```json
{
  "repo_path": "string",
  "since": "ISO-8601 | null",
  "min_confidence": 0.4
}
```

`since` defaults to 180 days ago. `min_confidence` filters out low-signal results (default 0.35).

Four staleness signals:

| Signal | Confidence | Trigger |
|---|---|---|
| `deleted_file` | 0.85 | A file linked to the decision existed but is no longer in the live file index |
| `missing_symbols` | 0.70 | A linked file exists but has no live symbols after re-ingest |
| `no_recent_activity` | 0.45 | The decision has commit links but none since `since` |
| `aged_adr` | 0.35 | Accepted ADR older than `since`, not superseded |

Returns:

```json
{
  "checked_at": "ISO-8601",
  "since": "ISO-8601",
  "stale_decisions": [
    {
      "decision_id": "uuid",
      "adr_id": "ADR-0042",
      "title": "string",
      "valid_from": "ISO-8601",
      "confidence": 0.85,
      "signals": [
        { "kind": "deleted_file", "detail": "src/old/path.rs", "confidence": 0.85 }
      ]
    }
  ],
  "warnings": []
}
```

---

### `trace_symbol_history`

Reconstruct the full architectural timeline for a named symbol: when it first appeared, which decisions and constraints governed it at each point, and which commits and episodes touched those decisions.

```json
{
  "repo_path": "string",
  "symbol": "string",
  "valid_from": "ISO-8601 | null",
  "valid_to": "ISO-8601 | null"
}
```

`valid_from` / `valid_to` bound the query window (default: all time). `symbol` is matched against the symbol name index.

Returns a timeline of events sorted oldest→newest:

| `event_type` | Meaning |
|---|---|
| `symbol_seen` | Symbol appeared (or disappeared) in the index |
| `decision_activated` | A governing decision became active |
| `decision_superseded` | A governing decision was closed |
| `constraint_activated` | A constraint became active on a governing decision |
| `constraint_superseded` | A constraint was closed |
| `commit` | A commit linked to a governing decision |
| `episode` | An episode linked to a governing decision |

```json
{
  "symbol": "OrderService",
  "current_file": "src/orders/service.rs",
  "first_seen": "ISO-8601",
  "timeline": [
    {
      "occurred_at": "ISO-8601",
      "event_type": "decision_activated",
      "entity_id": "uuid",
      "entity_type": "decision",
      "summary": "Use event sourcing for order lifecycle",
      "confidence": 0.95
    }
  ],
  "warnings": []
}
```

---

### `retract`

Soft-delete a hallucinated or incorrect LLM-extracted fact. Sets `valid_to = now` on the target entity (and cascades to its constraints). Idempotent: retracting an already-retracted entity adds a warning but does not error.

```json
{
  "repo_path": "string",
  "entity_type": "decision | constraint | episode",
  "entity_id": "uuid",
  "reason": "string",
  "replacement": "string | null"
}
```

* `entity_type = "decision"` — closes the decision and all of its active constraints.
* `entity_type = "constraint"` — closes only the named constraint.
* `entity_type = "episode"` — closes all decisions and their constraints that were derived from the episode.
* `replacement` — if provided, a retraction-correction episode is stored as an audit note (no LLM extraction is triggered); the episode ID is returned as `replacement_episode_id`.

Returns:

```json
{
  "retracted": true,
  "entity_type": "string",
  "entity_id": "uuid",
  "retracted_at": "string",
  "reason": "string",
  "decisions_closed": 1,
  "constraints_closed": 2,
  "replacement_episode_id": "uuid | null",
  "warnings": []
}
```

---

### `trace_call_path`

Traverses the call graph via `symbol_edges` to answer "who calls this?" (inbound) or "what does this call?" (outbound) up to a configurable depth. Replaces O(depth) grep-and-read loops with a single graph query.

```json
{
  "repo_path": "string",
  "symbol_name": "string",
  "direction": "outbound | inbound | both",
  "max_depth": 4,
  "min_confidence": 0.5,
  "valid_at": "string | null"
}
```

`direction` defaults to `"outbound"`. `max_depth` defaults to 4. `min_confidence` prunes edges below the threshold (default 0.5). Returns:

```json
{
  "root": { "name": "string", "kind": "string", "file": "string", "line": null },
  "chain": [
    { "name": "string", "kind": "string", "file": "string", "line": null, "via_edge": "calls", "depth": 1, "confidence": 0.95 }
  ],
  "truncated": false,
  "warnings": []
}
```

`truncated: true` when `max_depth` was reached and further nodes exist. Cycle detection prevents infinite loops on mutual recursion.

---

### `find_orphaned_code`

Reports files and symbols that have no linked architectural decisions or ADRs. Surfaces "dark matter" regions of the repository — code that has grown without architectural ownership.

```json
{
  "repo_path": "string",
  "path_prefix": "string | null"
}
```

`path_prefix` restricts the scan to a subdirectory relative to the repo root. Returns:

```json
{
  "orphaned_files": [{ "path": "string", "reason": "string" }],
  "orphaned_symbols": [{ "name": "string", "kind": "string", "file": "string", "line": null, "reason": "string" }],
  "total_files": 0,
  "total_symbols": 0,
  "warnings": []
}
```

Requires `ingest_symbols` and `sync_adrs_from_git` to have been run.

---

### `index_status`

Returns indexing freshness and embedding coverage for a repository. Distinguishes between information that truly does not exist and information that has not yet been indexed.

```json
{
  "repo_path": "string"
}
```

Returns per-entity status for ADRs (total, last sync), files (total, last ingested), and embeddable entities (decisions, constraints, episodes, commits, symbols): total count, embedded count, coverage ratio (0.0–1.0), and last ingestion timestamp.

---

### `validate_adr`

Validates an ADR file's structure and consistency against the repository's ingested knowledge graph.

```json
{
  "repo_path": "string",
  "adr_path": "string"
}
```

`adr_path` may be absolute or relative to `repo_path`. Returns:

```json
{
  "valid": false,
  "adr_id": "ADR-0042",
  "title": "string",
  "status": "proposed",
  "errors": ["string"],
  "warnings": ["string"]
}
```

Checks: missing required sections, duplicate ADR IDs, invalid supersession chains, referenced files/symbols not found in the index.

---

### `diff_architecture`

Temporal diff of architectural state between two timestamps or git refs. Shows which decisions, constraints, and ADRs were added or removed in a window.

```json
{
  "repo_path": "string",
  "from": "string",
  "to": "string | null"
}
```

`from` and `to` accept ISO-8601 timestamps or git refs (commit SHA, tag, branch name). `to` defaults to now. Returns:

```json
{
  "from_timestamp": "ISO-8601",
  "to_timestamp": "ISO-8601",
  "decisions_added": [{ "id": "uuid", "title": "string", "adr_id": "string", "timestamp": "ISO-8601" }],
  "decisions_removed": [],
  "constraints_added": [],
  "constraints_removed": [],
  "adrs_added": [],
  "adrs_removed": [],
  "summary": "string"
}
```

---

### `explain_answer`

Re-runs a query with full retrieval provenance — exposes which terms were extracted, which keyword/semantic/graph steps fired, and which decisions each step matched. Use for debugging retrieval quality and calibrating agent trust.

```json
{
  "repo_path": "string",
  "query": "string",
  "valid_at": "string | null"
}
```

Returns:

```json
{
  "query": "string",
  "terms_extracted": ["string"],
  "steps": [
    {
      "step": "keyword_search | semantic_search | graph_expansion | rrf_merge",
      "description": "string",
      "matched": [{ "decision_id": "uuid", "title": "string", "adr_id": "string", "score": 0.8, "reason": "string" }]
    }
  ],
  "final_decision_ids": ["uuid"],
  "warnings": []
}
```

---

### `sync_incremental`

Re-indexes only the files that have changed since a given timestamp or git ref. Runs `sync_adrs_from_git` for changed ADR files and `ingest_symbols` for changed source files. Prefer this over full re-runs on large repositories.

```json
{
  "repo_path": "string",
  "since": "ISO-8601 or git-ref | null",
  "adr_glob": "string | null"
}
```

`since` defaults to the most recent ingestion timestamp across all entity types; if the index is empty, indexes everything. `adr_glob` defaults to `"docs/adr/*.md"`. Returns:

```json
{
  "since_resolved": "ISO-8601",
  "changed_files": ["string"],
  "adrs_resynced": ["string"],
  "sources_reindexed": ["string"],
  "warnings": []
}
```

---

### `focused_file_brief`

Returns a compact, LLM-optimised brief for a single file or symbol in one call: exported symbols with line anchors, cross-file callers and callees grouped by file, governing architectural decisions, and recent commits. Cheaper than chaining `get_architecture` → `trace_call_path` → `find_decisions_for_code` separately.

```json
{
  "repo_path": "string",
  "file": "string | null",
  "symbol": "string | null"
}
```

Provide exactly one of `file` (repo-relative path) or `symbol` (name).

---

### `list_repos`

Lists every repository stored in the architectural memory database, with per-repo entity counts.

```json
{}
```

---

### `delete_repo`

Permanently deletes a repository and all associated data — symbols, decisions, constraints, commits, ADRs, edges, embeddings. Irreversible.

```json
{ "repo_path": "string" }
```

---

### `get_graph_snapshot`

Returns a node/edge snapshot of the knowledge graph for interactive relationship visualization (used by the manager client). Not a reasoning tool.

```json
{
  "repo_path": "string",
  "valid_at": "string | null"
}
```

---

### `reload_daemon`

Hot-reloads the running daemon: drains in-flight MCP sessions, rebuilds the server, and re-binds without dropping the listening socket, so clients see no `ECONNREFUSED` gap. Only available when weaver is running in `--daemon` mode; a no-op otherwise.

```json
{}
```

---

## Example

### Query

```json
{
  "query": "Why is the order service event-driven?",
  "scope": {
    "service": "order-service"
  }
}
```

### Response

```json
{
  "answer": "The order service uses event sourcing to ensure auditability and replayability per ADR-0042.",
  "decisions": [
    {
      "id": "ADR-0042",
      "title": "Use event sourcing for order lifecycle",
      "status": "accepted",
      "valid_from": "2023-04-10T00:00:00Z",
      "valid_to": null
    }
  ],
  "constraints": [
    {
      "id": "c-0091",
      "text": "Order state must never be mutated in place"
    }
  ],
  "evidence": [
    {
      "type": "commit",
      "ref": "a3f1d9b",
      "summary": "Introduced event store for order domain"
    }
  ],
  "warnings": [],
  "conflicts": [],
  "temporal_context": {
    "valid_at": null,
    "ingested_at": "2026-05-03T00:00:00Z"
  },
  "confidence": 0.91
}
```
