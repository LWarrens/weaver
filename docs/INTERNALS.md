# Internals

Storage schema, temporal model, graph model, and repository layout. See [TOOLS.md](TOOLS.md) for tool contracts and [DESIGN-claims-and-freshness.md](DESIGN-claims-and-freshness.md) for the claims layer.

## Storage

**SQLite** for MVP. The storage layer is abstracted so Postgres can replace it later without changing tool logic.

### Core Tables

| Table | Purpose |
|---|---|
| `repositories` | Tracked repositories |
| `adr_documents` | Parsed ADR markdown files |
| `decisions` | Extracted decision records linked to either ADRs or episodes |
| `constraints` | Obligations imposed by a decision |
| `files` | Tracked source files |
| `symbols` | Extracted code symbols |
| `symbol_edges` | Typed edges between symbols (CALLS, IMPORTS, contains) |
| `commits` | Git commits |
| `commit_files` | Files touched by each commit (co-change analysis) |
| `pull_requests` | Git PRs |
| `episodes` | Recorded architectural events |
| `entity_nodes` | Canonical entity identity and resolution for episode-sourced facts |
| `decision_code_links` | Decision ↔ file/symbol links |
| `decision_git_links` | Decision ↔ commit/PR links |
| `supersession_edges` | ADR supersession relationships |
| `temporal_edges` | Typed, time-bounded graph edges (applies_to, depends_on, conflicts_with, defines, imposes, evidences, mentions) |
| `communities` | Symbol clusters detected by label propagation |
| `community_members` | Symbol membership in communities |
| `routes` | HTTP route definitions extracted from source files |
| `claims` | Verifiable assertions decomposed from decisions and constraints |
| `evidence_anchors` | Content-hashed citation spans backing each claim |
| `evidence_verifications` | Append-only per-anchor freshness checks against the working tree |
| `index_lanes` | Per-lane index freshness (last commit, status, capabilities) |
| `freshness_manifests` | Cached per-view freshness evaluations |

Every meaningful entity includes:

```
id            — stable identity
source        — origin (git, adr, episode, inferred)
source_uri    — exact location in source
valid_from    — when the fact became true in the architecture
valid_to      — when the fact ceased to be true (null = still active)
ingested_at   — when this server learned about it
confidence    — 0.0–1.0
evidence_refs — references to supporting evidence
```

---

## Temporal Model
This system supports **bitemporal reasoning**.

| Field | Meaning |
|---|---|
| `valid_from` / `valid_to` | When the fact was true in the architecture |
| `ingested_at` | When this MCP server learned it |
| `source_time` | When the source artifact was authored or committed |

Historical facts are never destructively overwritten. When a fact is superseded, its `valid_to` is set. The old record is preserved.

---

## Graph Model

Architectural knowledge is stored in SQLite tables. `supersession_edges` is populated by ADR sync. `temporal_edges` is populated with ADR-typed edges (`applies_to`, `depends_on`, `conflicts_with`), `defines`/`imposes` edges from ADR sync and episode ingestion, and `evidences` Commit→Decision edges from `sync_commits_from_git`; it is traversed by `impact_of` and by `query`'s graph expansion. `symbol_edges` holds symbol-level edges (CALLS, IMPORTS, contains) emitted during `ingest_symbols`. Files remain the location/indexing layer for paths, hashes, ADR links, commits, and routes; symbol graph views keep file paths as symbol metadata instead of organizing symbols through synthetic file-to-symbol containment.

### Node Types

`ADR`, `Decision`, `Constraint`, `Repository`, `Commit`, `PR`, `File`, `Symbol`, `Module`, `Service`, `Episode`

### Edge Types

| Edge | Meaning |
|---|---|
| `defines` | ADR defines a Decision |
| `imposes` | Decision imposes a Constraint |
| `applies_to` | Constraint/Decision applies to a File/Symbol/Module/Service |
| `evidences` | Commit/Episode evidences a Decision |
| `modifies` | Commit modifies a File/Symbol |
| `affects` | Change affects a Decision area |
| `supersedes` | ADR supersedes another ADR |
| `mentions` | ADR mentions a File/Module/Service |
| `conflicts_with` | Decision conflicts with another |
| `supports` | Decision supports another |
| `depends_on` | Module/Service depends on another |
Every edge includes: `valid_from`, `valid_to`, `ingested_at`, `confidence`, `evidence_refs`.

---

## Project Structure

```text
src/
  main.rs
  server.rs             # MCP tool router (stdio + daemon share this)
  daemon.rs             # Streamable HTTP daemon + hot-reload handle
  embeddings.rs         # EmbeddingProvider trait + LM Studio / Ollama / OpenAI impls
  llm.rs                # LlmProvider trait + Ollama / OpenAI / Mock impls
  error.rs
  bin/

  tools/
    adr_lineage.rs
    architecture_query.rs
    check_consistency.rs
    delete_repo.rs
    diff_architecture.rs
    embed_all.rs
    explain_answer.rs
    find_decisions.rs
    find_orphaned_code.rs
    find_stale_decisions.rs
    focused_file_brief.rs
    freshness.rs
    generate_adr_draft.rs
    generate_adr_patch.rs
    get_architecture.rs
    get_graph_schema.rs
    get_graph_snapshot.rs
    impact_of.rs
    index_status.rs
    ingest_symbols.rs
    inspect_change.rs
    json_utils.rs
    list_repos.rs
    propose_links.rs
    record_episode.rs
    retract.rs
    sync_adrs.rs
    sync_commits.rs
    sync_incremental.rs
    synthesize_adr_leads.rs
    trace_call_path.rs
    trace_symbol_history.rs
    validate_adr.rs
    verify_claims.rs
    verify_evidence.rs
    verify_index_integrity.rs

  domain/
    entities.rs
    adr_parser.rs
    anchors.rs           # whitespace normalization, content/context hashing, anchor construction
    claims.rs            # claim builders + obligation-polarity detection
    completeness.rs      # read-set coverage check
    reconciliation.rs
    scoring.rs

  storage/
    mod.rs
    sqlite/           # SqliteStore split by query family
      mod.rs          # types, connect/migrations, row converters
      repositories.rs
      adrs.rs
      decisions.rs
      episodes.rs
      symbols.rs
      search.rs
      embeddings.rs
      call_paths.rs
      entities.rs
      git.rs
      graph.rs
      claims.rs         # claims, evidence anchors, verifications, index lanes, manifest cache
      tool_support.rs

  adapters/
    edges.rs
    registry.rs
    symbols.rs

migrations/           # 0001_initial.sql … 0019_drop_evidence_spans.sql (repo root)
scripts/
  install-weaver-task.ps1    # register the logon-start scheduled task (Windows)
  uninstall-weaver-task.ps1
  run-weaver-daemon.ps1      # daemon launcher with logging; used by the task
start.ps1             # dev: run daemon + manager-client dev server together
```

---

## Key Rules

### Temporal Integrity

* Historical facts are never destructively overwritten
* Supersession closes `valid_to` on the old fact and creates a new one
* `valid_at` queries use the bitemporal model — not "latest wins"

### Reconciliation

When sources disagree:

* Git wins for repository state
* ADR markdown wins for accepted decisions
* `accepted` > `proposed`
* `superseded` / `deprecated` ≠ current guidance
* Inferred relationships must be labeled with confidence
* Conflicts must be surfaced, not suppressed

### Writes Are Controlled

* No freeform memory
* No vague entities
* No duplicate decisions
* No implicit acceptance of decisions
* No invented links without confidence scores

---

## Non-Goals

* Not a chatbot
* Not a replacement for ADR files
* Not a general-purpose knowledge graph
* Not an autonomous architect
* Not a system that silently "figures things out"
* Does not use vector search as source of truth
* Does not collapse ADR status into a flat field without supersession edges

---

## Design Philosophy

Architecture decays without pressure.
Memory systems drift without constraints.
Agents will take shortcuts unless prevented.

This system enforces:

* explicit structure
* bitemporal evidence
* canonical truth sources
* constrained writes
* visible conflicts

---
