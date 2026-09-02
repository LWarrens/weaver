# weaver

weaver is an MCP server that gives a coding agent an architectural memory. It ingests your
ADRs, git history, and source symbols into one bitemporal SQLite store, then answers *"what
governs this code, and can we still prove it?"* with a citation trail instead of a guess.

Every answer carries its evidence - the ADR section, the commit, the symbol span - plus a
freshness verdict computed against your working tree. When the evidence behind a decision has
drifted, weaver tells you, or refuses the answer, rather than repeating a claim it can no
longer support.

This is not a chatbot. It's an architectural control plane.

## Why

Most systems carry three disconnected truths:

- **Code** shows what exists.
- **Git** shows what changed.
- **ADRs** show why it was decided.

Nobody reconciles them. Accepted decisions quietly go stale. Constraints get violated during a
change because nothing connected the constraint to the file being edited. Architectural intent
drifts from the implementation until the `docs/adr/` folder is archaeology. An agent told to
"follow the architecture" has no way to tell a live decision from a superseded one.

weaver keeps the three aligned, with SQLite as the bitemporal source of truth and every fact
tagged with when it was true, when weaver learned it, and how confident it is.

**An agent about to change your code.** Before it edits `orders/handler.rs`,
`inspect_change_against_decisions` checks the change against active constraints and - by
default - refuses to answer from evidence that has drifted, handing back the exact stale
anchors and the sync command to refresh them.

**A human doing archaeology.** `trace_symbol_history` reconstructs when a symbol appeared,
which decisions governed it at each point, and which commits and episodes touched them - a
timeline instead of a `git log` spelunk.

**CI drift detection.** `find_stale_decisions` and `verify_index_integrity` run in a pipeline
to flag accepted decisions whose files were deleted, whose symbols vanished, or whose cited
evidence no longer hashes to what the ADR claimed.

## What comes out

A query returns the answer *and* what backs it:

```json
{
  "answer": "The order service uses event sourcing for auditability and replayability, per ADR-0042.",
  "decisions": [
    { "id": "ADR-0042", "title": "Use event sourcing for order lifecycle",
      "status": "accepted", "valid_from": "2023-04-10T00:00:00Z", "valid_to": null }
  ],
  "constraints": [
    { "id": "c-0091", "text": "Order state must never be mutated in place" }
  ],
  "evidence": [
    { "type": "commit", "ref": "a3f1d9b", "summary": "Introduced event store for order domain" }
  ],
  "warnings": [],
  "conflicts": [],
  "temporal_context": { "valid_at": null, "ingested_at": "2026-05-03T00:00:00Z" },
  "confidence": 0.91
}
```

Nothing weaver stores is a bare string. Every claim is graded on two independent axes:

- **`evidence_grade`** - `unknown` / `partial` / `proven`, set once at ingest. Model-extracted
  text never enters at `proven`.
- **`freshness`** - `fresh` / `stale`, recomputed against your working tree from the content
  hash of the cited span.

From those, a claim's **`disposition`** is `unaffected` (all citations still hash clean),
`affected` (stale citations were relocated), or `unprovable` - a citation moved and could not
be found again, or the claim was never anchored. `unprovable` is terminal, not "low
confidence." Under `verify: strict`, an in-scope stale claim populates `refused` with a
rebuild obligation instead of returning a normal answer.

## Quickstart

```bash
cargo build --release

# Run the daemon, indexing a repo on the way up
./target/release/weaver \
  --db ./arch.db --daemon --bind 127.0.0.1:8444 \
  --index-repo /path/to/your/repo
```

Point an MCP client at `http://127.0.0.1:8444/mcp`, then:

```jsonc
// once, and again whenever ADRs change
sync_adrs_from_git   { "repo_path": "/path/to/your/repo", "adr_glob": "docs/adr/*.md" }

// what governs this file?
find_decisions_for_code { "repo_path": "/path/to/your/repo",
                          "target": { "file": "src/orders/handler.rs" } }
```

The SQLite file is created on first run. Embeddings and LLM fact-extraction are optional (see
[Configuration](#configuration)); without them, `query` runs keyword + graph retrieval and
episodes are stored without extraction.

## Core concepts

- **SQLite is the source of truth.** Not the graph, not the vector index - those are derived
  query surfaces. One writer, WAL, every row bitemporal.
- **ADRs are evidence, not memory.** weaver reads `docs/adr/*.md` as human-facing evidence for
  accepted decisions; the decisions live in the database with supersession edges, so
  `accepted` and `superseded` never collapse into one flat field.
- **Bitemporal.** `valid_from` / `valid_to` (true in the architecture) are tracked separately
  from `ingested_at` (when weaver learned it). History is closed, never overwritten.
- **Claims + anchors.** Decisions and constraints decompose into individually verifiable
  claims, each pinned to a content-hashed span of an ADR section, a commit, or a symbol.
- **Retrieval is three retrievers.** `query` fuses keyword (FTS), semantic (embeddings), and
  graph (BFS over typed temporal edges) with Reciprocal Rank Fusion.
- **Nothing is auto-promoted.** Inferred links carry confidence scores and stay candidates.
  Discussion is never silently upgraded to an accepted decision. LLM mistakes are `retract`-able
  with an audit trail.

## Tools

37 MCP tools, all returning strict JSON. Full request/response contracts:
**[docs/TOOLS.md](docs/TOOLS.md)**.

| Group | Tools |
|---|---|
| **Ingest** | `sync_adrs_from_git`, `ingest_symbols` (+ `_status`, `cancel_ingest`), `sync_commits_from_git`, `sync_incremental`, `record_decision_episode`, `embed_all` |
| **Ask** | `query`, `find_decisions_for_code`, `get_architecture`, `focused_file_brief`, `trace_call_path`, `impact_of`, `explain_answer` |
| **Verify** | `inspect_change_against_decisions`, `verify_claims`, `verify_index_integrity`, `find_stale_decisions`, `check_consistency`, `index_status`, `validate_adr` |
| **History** | `trace_symbol_history`, `adr_lineage`, `diff_architecture` |
| **Author** | `generate_adr_draft`, `generate_adr_patch`, `synthesize_adr_leads` (+ `_status`) |
| **Curate** | `propose_links`, `retract`, `find_orphaned_code`, `list_repos`, `delete_repo` |
| **Schema / ops** | `get_graph_schema`, `get_graph_snapshot`, `reload_daemon` |

## The manager client

`manager-client/` is a browser UI (Svelte + Vite) that connects straight to the daemon's
Streamable HTTP endpoint. It renders the knowledge graph as a 3D force-directed view -
decisions, files, symbols, commits, constraints, and observed-pattern leads, colored by kind
and edge type - and exposes every tool as a form with results inline.

<!-- media: replace with a real screenshot / clip once captured -->
<!-- ![weaver manager client - 3D knowledge graph view](docs/media/manager-client.png) -->

_Screenshot to come._

```bash
./start.ps1          # daemon (debug) + Vite dev server on :5173, from an external terminal
```

or `cd manager-client && npm install && npm run dev` against an already-running daemon.

## Building

```bash
cargo build --release
# binary: target/release/weaver
```

Requires a recent stable Rust toolchain.

## Running

```bash
# stdio (one-off clients)
cargo run -- --db ./arch.db

# Streamable HTTP daemon (persistent clients - preferred)
cargo run -- --db ./arch.db --daemon --bind 127.0.0.1:8444
#   endpoint: http://127.0.0.1:8444/mcp
```

`--index-repo /path/to/repo` (or `WEAVER_INDEX_REPO`) runs `ingest_symbols` once at startup,
before serving requests. `--config .weaver.yaml` loads settings from YAML; with no config
path, CLI flags, or `WEAVER_*` env vars, weaver looks for `.weaver.yaml` in the working
directory. CLI flags and env vars override YAML.

```yaml
# .weaver.yaml
db: ./arch.db
daemon: true
bind: 127.0.0.1:8444

embedding_provider: lmstudio      # lmstudio | ollama | openai
embedding_url: http://localhost:1234
embedding_model: text-embedding-3-small

llm_provider: lmstudio            # ollama | openai | mock
llm_url: http://localhost:1234
llm_model: bonsai-1.7b

projects:
  - path: .
    pattern: src/**/*.rs
```

On Windows, weaver is not a service - the daemon calls a `localhost` embedding/LLM provider
that lives in your interactive session. Use the logon-start scheduled task instead:

```powershell
cargo build --release
.\scripts\install-weaver-task.ps1     # -RunNow to also start it now
```

## Configuration

Both AI integrations are optional and degrade gracefully when unset.

| Variable | Purpose |
|---|---|
| `WEAVER_EMBEDDING_PROVIDER` | `lmstudio` / `ollama` / `openai` - powers semantic search in `query` |
| `WEAVER_EMBEDDING_URL`, `WEAVER_EMBEDDING_MODEL` | provider endpoint and model |
| `WEAVER_LLM_PROVIDER` | `ollama` / `openai` / `mock` - powers episode fact extraction and lead synthesis |
| `WEAVER_LLM_MODEL`, `WEAVER_OLLAMA_URL` | provider model and endpoint |
| `OPENAI_API_KEY` | required for the `openai` provider |
| `WEAVER_DB`, `WEAVER_BIND`, `WEAVER_INDEX_REPO` | daemon startup without flags |

With no embedding provider, `query` falls back to keyword + graph retrieval. With no LLM
provider, episodes are still recorded - only fact/relationship extraction is skipped.

## Connecting an MCP client

```jsonc
// VS Code (Copilot) - Streamable HTTP server entry
{ "servers": { "weaver": { "type": "http", "url": "http://127.0.0.1:8444/mcp" } } }

// Claude Desktop
{ "mcpServers": { "weaver": { "url": "http://127.0.0.1:8444/mcp" } } }
```

Stdio remains available for clients that cannot attach to Streamable HTTP; it is not the
preferred long-running workflow.

## Scope & non-goals

**weaver does:**

- parse ADRs, source symbols (Rust, TS/JS, Python, Go, Java, C/C++, C#, Ruby, plus config and
  doc formats), git commits, and decision episodes into one bitemporal store
- resolve which decisions and constraints govern a file, symbol, module, or service
- verify cited evidence against the working tree and grade each claim's freshness
- fuse keyword, semantic, and graph retrieval; expose the full retrieval provenance
- generate ADR drafts and patches - **returned, never written to your repo**

**weaver does not:**

- act as a chatbot or an autonomous architect - it returns structured evidence, not opinions
- treat vector similarity as truth
- silently promote discussion to an accepted decision, or invent links without a confidence score
- collapse ADR status into a flat field without supersession edges
- modify your repository

**Rough edges / WIP:**

- Storage is SQLite only, single-writer. The layer is abstracted for Postgres later.
- Call-graph and route extraction are heuristic (tree-sitter AST patterns + regex), not a full
  name resolver - which is why those edges carry confidence.
- `find_decisions_for_code` module/service resolution leans on entity-name matching and a
  path-segment fallback; it flags matches that came only from the fallback.
- LLM fact-extraction quality tracks whatever model you point it at.

## Project status

Phases 1-4 are implemented: ADR / symbol / commit / episode ingestion, three-retriever
`query`, the temporal graph, and claims with content-hashed evidence anchors and per-view
freshness manifests. Changelog: [docs/STATUS.md](docs/STATUS.md). Internals (schema, temporal
and graph models, layout): [docs/INTERNALS.md](docs/INTERNALS.md). Design of the claims layer:
[docs/DESIGN-claims-and-freshness.md](docs/DESIGN-claims-and-freshness.md).

## License

Dual-licensed under **MIT OR Apache-2.0** (SPDX: `MIT OR Apache-2.0`). You may use this
software under the terms of either license. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
