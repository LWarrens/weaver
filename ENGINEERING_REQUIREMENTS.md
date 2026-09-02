# Engineering Requirements

## Status

Version: 0.2  
Date: 2026-06-05  
Product name: Weaver  
Package name: `weaver`

This document defines engineering requirements for the Weaver library and server. It is descriptive of the current product intent and constrains future implementation work. It does not introduce new components by itself.

## Purpose

Weaver must provide a strict MCP-accessible architectural memory system that connects code, git history, ADRs, decision episodes, and temporal facts into evidence-backed answers.

The library exists to help agents and engineers answer architecture questions without treating stale decisions, inferred relationships, or vector matches as authoritative truth.

## Scope

In scope:

- A Rust MCP server exposing architectural memory tools.
- SQLite-backed storage for bitemporal architectural facts.
- ADR ingestion from git repositories.
- Source symbol, route, call-edge, and community ingestion from source files.
- Git commit ingestion and linking to decisions.
- Episode recording for discussions, PR comments, and meeting notes.
- Optional embedding-backed semantic retrieval.
- Optional LLM-backed fact extraction and ADR lead synthesis.
- Query, analysis, correction, and authoring tools exposed through MCP.
- Streamable HTTP daemon mode and stdio-compatible MCP operation.
- A local manager client for inspecting repository memory and graph snapshots.

Out of scope:

- Replacing human-authored ADRs.
- Acting as an autonomous architecture decision maker.
- Treating vector similarity as source of truth.
- Creating authoritative links without confidence and provenance.
- General-purpose knowledge graph storage unrelated to software architecture.
- Silent fallback behavior that hides missing ingestion, missing providers, or stale data.

## Users

Primary users:

- LLM coding agents that need compact, evidence-backed architectural context.
- Engineers reviewing changes against known architectural decisions.
- Maintainers curating ADRs, decision episodes, and stale architectural facts.

Secondary users:

- Tooling authors integrating Weaver through MCP.
- Engineers visually inspecting graph/debug state through the manager client.

## Design Constraints

- SQLite is the runtime source of truth for architectural memory.
- Markdown ADRs are human-facing evidence, not the primary storage layer.
- Git history is the chronology and implementation evidence trail.
- Historical facts must not be destructively overwritten.
- Every answer must be traceable to stored decisions, constraints, commits, files, symbols, episodes, routes, or warnings.
- Inferred relationships must carry confidence and provenance.
- Retrieval tools and graph/debug UI tools must remain conceptually separate.
- File paths must be normalized and must not escape the requested repository root.
- Missing providers, stale indexes, and unsupported query modes must be surfaced as warnings.

## Functional Requirements

### Repository State And Administration

REQ-001: Weaver must report indexed repository state with counts, embedding coverage, and last-ingestion timestamps per entity lane.

Acceptance:

- `index_status` reports ADR, file, decision, constraint, episode, commit, symbol, and entity-node coverage when available.
- A caller can distinguish "no data exists" from "data has not been indexed."
- Stale or incomplete lanes are reported as warnings when known.

REQ-002: Weaver must summarize ingested architecture for a repository.

Acceptance:

- `get_architecture` returns active decisions, counts, symbol communities, temporal context, and warnings.
- The response must be scoped to the requested repository.
- The response must not imply completeness when required ingestion has not run.

REQ-003: Weaver must expose schema-level information for debugging and integration.

Acceptance:

- `get_graph_schema` reports known storage tables, roles, columns, graph node tables, and edge tables.
- Optional row counts can be included without changing schema semantics.

REQ-004: Weaver must expose tools for administering stored repositories.

Acceptance:

- `list_repos` returns all repositories stored in the database with their path, id, and last ingestion timestamp.
- `delete_repo` accepts an explicit repository identifier and permanently removes all associated data including decisions, constraints, symbols, commits, episodes, and ADRs.
- `delete_repo` does not use soft deletion; removal is immediate and irreversible.

### ADR Ingestion And Validation

REQ-010: Weaver must ingest Markdown ADR files from a repository.

Acceptance:

- `sync_adrs_from_git` accepts `repo_path` and `adr_glob`.
- It parses ADR id, title, status, date, context, decision, consequences, supersession references, constraints, and affected files/modules/services when present.
- It upserts ADR documents, decisions, constraints, decision-code links, and supersession edges.
- Re-running unchanged ADRs is idempotent.
- Missing fields are represented as uncertain or invalid, not invented.

REQ-011: Weaver must validate an ADR file without mutating storage.

Acceptance:

- `validate_adr` accepts absolute or repo-relative ADR paths.
- It reports missing required sections, duplicate ADR IDs, unknown statuses, and invalid supersession references.
- It returns `valid = true` only when there are no validation errors.

REQ-012: Weaver must support ADR authoring without directly modifying files.

Acceptance:

- `generate_adr_draft` creates a Markdown draft with the next sequential ADR ID and related-decision context.
- `generate_adr_patch` returns a unified diff for creating or updating an ADR.
- Neither tool writes files or applies patches.

### Code And Symbol Ingestion

REQ-020: Weaver must ingest source symbols using tree-sitter.

Acceptance:

- `ingest_symbols` accepts `repo_path`, optional pattern, and force behavior.
- The tool runs as a background job and returns a job id immediately.
- `ingest_symbols_status` reports job state and progress.
- `cancel_ingest` requests clean cancellation after the current file.
- The ingest path honors ignore rules and skips unsupported or generated/build artifacts.

REQ-021: Weaver must preserve current symbol truth without leaving ghost records.

Acceptance:

- Changed files retire old live symbols by setting `valid_to`.
- Unchanged files can be skipped using content hashes.
- Deleted or renamed symbols must not remain live after re-ingestion.

REQ-022: Weaver must extract useful code graph metadata.

Acceptance:

- Symbol records include file, line span, kind, signature, return type, visibility, async flag, complexity, and decorators when available.
- `symbol_edges` includes call/import edges and supported containment edges.
- Route detection records framework, method, path, source file, and handler id when resolvable.
- Community detection groups symbols from call/import edges without requiring an LLM.

### Git Ingestion

REQ-030: Weaver must ingest git commits into architectural memory.

Acceptance:

- `sync_commits_from_git` accepts repository, branch, since timestamp, and limit.
- Commits are idempotently stored by SHA.
- Explicit ADR mentions create high-confidence decision links.
- Keyword overlap may create lower-confidence decision links.
- Commit ingestion must not require a network connection.

REQ-031: Weaver must support incremental indexing.

Acceptance:

- `sync_incremental` accepts an ISO-8601 timestamp or git ref.
- It detects changed files from git history.
- It re-syncs changed ADR files and re-indexes changed source files only.
- Omitted `since` resolves from known ingestion timestamps, and the chosen baseline is reported.

### Decision And Governance Queries

REQ-040: Weaver must resolve governing decisions for code.

Acceptance:

- `find_decisions_for_code` supports file and symbol targets.
- File lookup uses normalized repo-relative paths.
- Symbol lookup resolves matching files, then governing decisions.
- Module and service lookup resolves through entity nodes and decision mentions edges; module lookup may additionally match decisions linked to files under a matching path segment.
- An unresolved module or service name must warn rather than fabricate results.
- Route information is included for files with indexed routes.

REQ-041: Weaver must inspect proposed changes against active decisions.

Acceptance:

- `inspect_change_against_decisions` accepts changed files, optional symbols, optional summary, and optional `valid_at`.
- It checks active decisions and constraints for possible violations.
- It reports matched constraints with confidence and relevant decision metadata.
- It must prefer warnings and low confidence over false certainty.

REQ-042: Weaver must detect stale or drifted decisions.

Acceptance:

- `find_stale_decisions` reports accepted decisions with drift signals such as deleted files, missing symbols, no recent linked activity, and old unsuperseded ADRs.
- Results include signal kind, detail, confidence, and checked timestamp.

REQ-043: Weaver must identify architectural dark matter.

Acceptance:

- `find_orphaned_code` reports files and symbols without linked decisions or ADRs.
- It supports optional path-prefix scoping.
- It warns when symbol ingestion has not produced the data required for meaningful results.

### Retrieval And Provenance

REQ-050: Weaver must query architecture using keyword, semantic, and graph expansion where available.

Acceptance:

- `query` merges keyword, semantic, and temporal graph results using a deterministic ranking strategy.
- Semantic retrieval is skipped with a warning when no embedding provider is configured.
- Graph expansion depth is bounded.
- Results include decisions, constraints, confidence, warnings, and temporal context.

REQ-051: Weaver must explain retrieval provenance.

Acceptance:

- `explain_answer` reports extracted terms, keyword matches, semantic matches, graph expansion matches, and final ranking.
- The provenance must be detailed enough for an agent to justify why a decision appeared in a response.

REQ-052: Weaver must provide compact file or symbol briefs.

Acceptance:

- `focused_file_brief` accepts either a repo-relative file or a symbol.
- It returns exports, cross-file callers, cross-file callees, governing decisions, and recent commits.
- The response should be compact enough to replace multi-tool discovery loops for common LLM workflows.

### Claims And Evidence Freshness

See `docs/DESIGN-claims-and-freshness.md`.

REQ-053: Weaver must decompose decisions and constraints into individually verifiable claims.

Acceptance:

- ADR sync and episode ingestion create a `claims` row per decision and per constraint, carrying `kind`, obligation `polarity` (constraints), `evidence_grade`, a `read_set`, and bitemporal timestamps.
- `evidence_grade` is set at ingest and is independent of freshness; model-derived claims never enter at `proven`.
- Retracting a decision, constraint, or episode closes its claims (sets `valid_to`) rather than deleting them.

REQ-054: Weaver must anchor every claim to content-hashed evidence spans.

Acceptance:

- Each `evidence_anchors` row records canonical identity `(source_kind, source_uri, subpath)`, a `locator`, the anchored text, and `content_hash = sha256(normalize_ws(text))` over the span only.
- Anchor insertion is idempotent on `(claim_id, source_kind, source_uri, subpath, content_hash)`.
- A claim whose `read_set` is not fully covered by its anchors is reported as an incomplete claim.

REQ-055: Weaver must verify anchors against the current working tree and record each check.

Acceptance:

- `evidence_verifications` is append-only; each row carries the resolved `repo_commit`, an `edit_class` (`unchanged`/`shifted`/`affected`/`deleted`), a `freshness` (`fresh`/`stale`), and any relocated locator.
- Immutable sources (episodes, PRs, commits) verify as fresh without filesystem access.
- Re-ingestion re-verifies open anchors off the query path.

REQ-056: Weaver must assign each claim a three-state disposition.

Acceptance:

- `unaffected` — every anchor fresh; `affected` — all stale anchors relocated; `unprovable` — any stale anchor not relocated, or zero anchors.
- `unprovable` is terminal and must not be presented as low confidence.
- `verify_claims` returns per-anchor freshness, per-claim disposition, and completeness for an ADR, decision, file, or symbol target.

REQ-057: Weaver must attach a per-view freshness manifest to retrieval responses and refuse stale strict verification.

Acceptance:

- `query`, `find_decisions_for_code`, and `inspect_change_against_decisions` accept a `verify` mode (`cached`/`fresh`/`skip`/`strict`) and attach a `freshness` manifest aggregating dispositions, stale claims, incomplete claims, and index-lane lag.
- `verify: strict` sets a refusal with a rebuild obligation (naming drifted anchors and the sync commands to run) when any in-scope claim is stale; `inspect_change_against_decisions` defaults to strict.
- `verify: skip` attaches no manifest and adds a warning.

REQ-058: Weaver must provide an offline index-integrity oracle.

Acceptance:

- `verify_index_integrity` rebuilds the ADR lane in a throwaway store and diffs its declared-fact projection against the live index, reporting `clean`/`divergent`/`not_audited` per lane plus the diverging projections.
- It runs off the query hot path and is exercised in CI.
- `index_status` reports per-lane last-indexed commit, commit lag versus HEAD, status, and enabled query capabilities.

### Temporal And Graph Analysis

REQ-060: Weaver must support impact traversal from architectural decisions.

Acceptance:

- `impact_of` traverses active temporal edges from an ADR or decision.
- Traversal respects edge type filters, max depth, confidence, and `valid_at`.
- Cycles must not cause infinite traversal.

REQ-061: Weaver must trace call paths through indexed symbol edges.

Acceptance:

- `trace_call_path` supports inbound, outbound, and both directions.
- It resolves exact symbol names first and suffix matches only when unambiguous.
- Traversal is bounded by max depth and confidence.
- The result indicates truncation when more reachable nodes exist.

REQ-062: Weaver must trace symbol history across architectural time.

Acceptance:

- `trace_symbol_history` returns a chronological timeline for a symbol.
- Timeline events may include symbol sightings, decision activations, supersession, constraints, linked commits, and episodes.
- The result must include warnings when the symbol cannot be resolved or the index is stale.

REQ-063: Weaver must diff architectural state across time.

Acceptance:

- `diff_architecture` accepts timestamps or git refs.
- It reports decisions, constraints, and ADRs added or removed in the interval.
- Git refs resolve through local git metadata.

REQ-064: Weaver must expose ADR lineage.

Acceptance:

- `adr_lineage` walks supersession edges in both directions.
- It reports ancestors, descendants, current authority, and cycle warnings.

REQ-065: Weaver must explain cross-ADR conflicts without resolving them.

Acceptance:

- `check_consistency` reports explicit `conflicts_with` edges, contradictory constraint pairs (opposite obligation polarity over shared terms), and supersession inconsistencies.
- Every reported conflict carries its own confidence and a human-readable explanation naming the ADRs or decisions involved.
- The tool is read-only: it never closes, retracts, or rewrites any record.

### Episodes, Inference, And Correction

REQ-070: Weaver must record architectural episodes.

Acceptance:

- `record_decision_episode` stores source, optional source URI, occurrence time, content, and structured decisions.
- Episode-backed decisions link directly to the episode and do not create synthetic ADR documents.
- If an LLM provider is configured, fact extraction may add decisions, constraints, entities, or relationships.
- If no LLM provider is configured, the episode is still stored and fact extraction is skipped with a warning.

REQ-071: Weaver must synthesize ADR leads only as observed patterns.

Acceptance:

- `synthesize_adr_leads` operates on orphaned code candidates.
- It must describe what code already does, not propose new architecture.
- It runs in the background and is polled through `synthesize_adr_leads_status`.
- It requires an LLM provider and warns when none is configured.
- Recorded leads must have episode provenance.

REQ-072: Weaver must propose links without promoting them.

Acceptance:

- `propose_links` returns candidate links to commits, symbols, decisions, and routes.
- Each candidate includes confidence and reason.
- No candidate is written as authoritative by this tool.

REQ-073: Weaver must correct incorrect facts through soft deletion.

Acceptance:

- `retract` accepts decision, constraint, or episode targets.
- It sets `valid_to` rather than deleting rows.
- Cascading behavior must be explicit for episode and decision retractions.
- Repeated retraction is idempotent and returns a warning.

### Embeddings And LLM Providers

REQ-080: Weaver must make embeddings optional.

Acceptance:

- The system works without an embedding provider.
- Embedding-backed tools warn and degrade to keyword or no-op behavior as appropriate.
- `embed_all` backfills decisions, constraints, episodes, commits, symbols, and entity nodes that lack embeddings.
- Empty texts are skipped without failing the whole run.

REQ-081: Weaver must support configured embedding providers.

Acceptance:

- Supported providers include LM Studio, Ollama, and OpenAI.
- Provider configuration is read from CLI options, YAML config, or `WEAVER_*`/provider-specific environment variables.
- Chunked embedding must produce a single vector for long text.

REQ-082: Weaver must make LLM extraction optional and explicit.

Acceptance:

- Supported LLM providers include Ollama, OpenAI, and mock.
- LLM-backed tools must warn when no provider is configured.
- Mock provider behavior must support deterministic tests.
- LLM output must be parsed defensively and must not silently become accepted ADR truth.

### MCP Server, CLI, And Daemon

REQ-090: Weaver must expose tools through MCP.

Acceptance:

- The server uses `rmcp` tool routing.
- Tools return structured JSON or clearly structured text containing JSON.
- Server instructions summarize orientation, ingestion, query, analysis, authoring, correction, and utility workflows.

REQ-091: Weaver must support long-running daemon operation.

Acceptance:

- Daemon mode serves Streamable HTTP at `/mcp`.
- Clients can preserve MCP session IDs across requests.
- `reload_daemon` is available only in daemon mode and drains active sessions before restart.

REQ-092: Weaver must support configuration from CLI, environment, and YAML.

Acceptance:

- CLI options override environment variables.
- Environment variables override YAML defaults.
- Relative paths in YAML resolve from the YAML file location.
- Startup indexing can be configured with `--index-repo` or `WEAVER_INDEX_REPO`.
- A `projects[]` array in YAML config allows multiple repositories to be indexed at startup; each entry accepts `path` and an optional `pattern`.

### Manager Client

REQ-100: Weaver may provide a local manager client for inspection.

Acceptance:

- The client is an inspection/debug surface, not the authoritative memory layer.
- Graph snapshots shown in the client are built by `get_graph_snapshot` (REQ-101) and must not imply storage relationships that do not exist.
- UI-only projections must be clearly distinguished from persisted graph facts.

REQ-101: Weaver must expose a graph snapshot tool to support manager client visualization.

Acceptance:

- `get_graph_snapshot` returns a node-edge snapshot scoped to a repository.
- Snapshot nodes and edges must reflect persisted storage relationships only.
- The response must be scoped to the requested repository.

## Data Requirements

DATA-001: Every meaningful entity must include stable identity, source, source URI when available, valid time, ingestion time, confidence, and evidence references where supported.

DATA-002: The storage model must preserve bitemporal meaning:

- `valid_from` and `valid_to` represent when a fact is true in the architecture.
- `ingested_at` represents when Weaver learned the fact.
- `source_time` represents when the source artifact was authored or committed.

DATA-003: Authoritative graph relationships must be stored in SQLite tables. Runtime-only UI projections must not be treated as storage truth.

DATA-004: Repository scoping must be explicit. Queries and mutations must not cross repository boundaries unless a future requirement explicitly introduces that behavior.

DATA-005: Confidence must be carried on inferred facts and candidate relationships. A missing confidence score is invalid for inferred data.

## Component Roles

These roles describe existing implementation areas. Any new component must satisfy AGENTS.md's spec-reference and single-role requirements before it is introduced.

| Component | Requirement anchor | Role |
|---|---|---|
| `src/server.rs` | REQ-090, REQ-091 | Expose storage-backed operations as MCP tools and daemon lifecycle hooks. |
| `src/main.rs` | REQ-092 | Parse runtime configuration and start the requested server mode. |
| `src/storage/sqlite/` | DATA-001 through DATA-004 | Persist and query bitemporal architectural memory in SQLite. |
| `migrations/` | DATA-001 through DATA-004 | Version the SQLite schema required by storage behavior. |
| `src/domain/adr_parser.rs` | REQ-010, REQ-011 | Parse ADR Markdown into structured fields. |
| `src/domain/entities.rs` | DATA-001, DATA-002 | Define serializable domain entities shared by tools and storage. |
| `src/domain/anchors.rs` | REQ-054 | Whitespace normalization, content/context hashing, and anchor construction. |
| `src/domain/claims.rs` | REQ-053 | Build decision/constraint/observation claims and detect obligation polarity. |
| `src/domain/completeness.rs` | REQ-054 | Compute the read-set identities a claim's anchors do not cover. |
| `src/storage/sqlite/claims.rs` | REQ-053 through REQ-058 | Persist and query claims, evidence anchors, verifications, index lanes, and manifest cache. |
| `src/tools/freshness.rs` | REQ-055 through REQ-058 | Resolve claim states, build/attach freshness manifests, and compute lane statuses and rebuild obligations. |
| `src/tools/verify_evidence.rs` | REQ-055 | Verify a single anchor against the working tree and classify its edit. |
| `src/tools/verify_claims.rs` | REQ-056 | Report per-anchor freshness and per-claim disposition for a target. |
| `src/tools/verify_index_integrity.rs` | REQ-058 | Diff a from-scratch ADR-lane rebuild against the live index. |
| `src/tools/` | REQ-001 through REQ-004, REQ-010 through REQ-073, REQ-101 | Implement individual MCP tool behaviors. |
| `src/adapters/registry.rs` | REQ-020 | Select supported tree-sitter language parsers. |
| `src/adapters/symbols.rs` | REQ-020, REQ-022 | Represent extracted source symbols. |
| `src/adapters/edges.rs` | REQ-022, REQ-061 | Extract call/import/containment edges from parsed source files. |
| `src/adapters/routes.rs` | REQ-022, REQ-040 | Detect HTTP routes and route-handler metadata. |
| `src/embeddings.rs` | REQ-080, REQ-081 | Provide optional embedding backends and vector utilities. |
| `src/llm.rs` | REQ-071, REQ-082 | Provide optional text-generation backends for extraction workflows. |
| `src/daemon.rs` | REQ-091 | Serve the Streamable HTTP MCP daemon. |
| `manager-client/` | REQ-100, REQ-101 | Inspect repository memory and graph/debug state. |

## Non-Functional Requirements

NFR-001: Reliability

- Tool failures must surface actionable errors or warnings.
- Background jobs must expose status and completion state.
- Idempotent ingest operations must remain safe to re-run.

NFR-002: Traceability

- Answers must include enough identifiers, temporal context, and warnings for a caller to audit the result.
- Candidate or inferred links must not be indistinguishable from accepted architectural truth.

NFR-003: Safety

- Tools must validate repository paths and generated patch paths.
- Tools must not apply generated ADR patches automatically.
- Delete and retract operations must require explicit target identifiers.

NFR-004: Performance

- Incremental indexing must avoid full repository re-ingestion when changed-file information is available.
- Symbol ingestion must skip unchanged files by content hash unless forced.
- Embedding backfills may run concurrent provider requests but must collect item-level failures.

NFR-005: Testability

- Provider traits must allow deterministic mock behavior.
- Tool logic should be testable against temporary repositories and SQLite databases.
- New behavior must include focused tests proportional to its risk and blast radius.

NFR-006: Portability

- The core server must run on Windows, macOS, and Linux where Rust, SQLite, git, and configured providers are available.
- The server must not require `DATABASE_URL` at compile time.

## Verification Requirements

VER-001: Rust checks

- `cargo test --quiet` must pass before release.
- `cargo build` must pass without a compile-time database URL.

VER-002: Manager client checks

- When manager-client behavior changes, run the package's type/build checks from `manager-client/`.
- Graph/debug UI changes must be visually verified when layout or interaction behavior changes.

VER-003: Documentation checks

- README and technical behavior documentation must not claim support beyond implemented behavior.
- Public tool lists must match `src/server.rs` registrations.
- Any new component documented here must include a requirement anchor and role.

VER-004: Migration checks

- New migrations must be append-only.
- Existing migrations must not be edited after release without an explicit data repair requirement.
- Storage tests must cover meaningful schema behavior.

## Open Requirements

These are known future requirements, not current behavior:

- Pull request ingestion and linking.

Each open requirement needs an implementation issue or ADR before new components are added for it.
