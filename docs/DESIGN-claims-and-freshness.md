# Design: Claims, Content-Hashed Anchors, Per-Evidence Freshness, Per-View Freshness Manifests

## Status

Draft v2 — Phase 4 candidate. Supersedes the "Open Requirements" in
`ENGINEERING_REQUIREMENTS.md` (evidence-span population) and the tracked gap
"`evidence_spans` population: not implemented" in `TECHNICAL_BEHAVIOR.md`.

v2 revision incorporates two pieces of prior art:

- **EA-Graph** (arxiv 2608.04278) — *Artifact-Anchored Verification Memory for
  Coding Agents*. Content-hashed anchors bind claims to artifact spans; evidence
  grade and freshness are tracked as **independent axes**; queries over stale
  facts are **refused with a rebuild obligation**, and claims resolve to one of
  three states (`unaffected` / `affected` / `unprovable`).
- **CodeNib** (arxiv 2607.25431) — *Multi-View Repository Context System*. A
  per-commit **manifest** catalogs each index view's commit, `lag`, status, and
  capabilities; views fail in isolation; incremental maintenance classifies
  edited units as `deleted / affected / shifted / unchanged / added` and rebases
  locations; an **offline output-equality check** qualifies incremental
  correctness off the serving path.

## Problem

Weaver already refuses to "return vibes": every answer is meant to be traceable
to a decision, constraint, commit, file, symbol, or episode. But traceability
today stops at the *record* level:

- `evidence_refs` is a JSON array of UUIDs pointing at *other rows*, not at the
  exact text that justified the fact.
- `evidence_spans` (the table for cited text ranges) has never been populated.
- Freshness is one repo-wide heuristic: `stale_index_warning` compares the last
  ingest timestamp to `git HEAD`. It cannot say *which* decision is stale, *why*,
  or *by how much*.
- `confidence` is a single float that conflates two unrelated things: how well
  the fact was *grounded* when recorded (deterministic ADR parse vs. LLM guess)
  and whether the cited code *still looks the way it did*.
- A caller cannot tell, for a given answer, whether to trust it, re-read the
  code first, or treat it as unrecoverable.

Result: "ADR-0042 governs this file" is returned with identical authority
whether the cited function is untouched, shifted 40 lines down, edited, or
deleted last week.

## What we adopt from prior art

| Concept | Source | How Weaver adopts it |
|---|---|---|
| Claims as the unit of verification | EA-Graph | `claims` table; decisions/constraints/leads decompose into claims |
| `(store, path, subpath)` canonical anchor identity, alias→leaf resolution | EA-Graph | Anchor identity = `(source_kind, source_uri, subpath)`; symbol subpaths resolve through re-exports / `entity_nodes` to the leaf qualified name **before** hashing |
| Content hash over the **anchored span**, not the whole file | EA-Graph | `content_hash = sha256(normalize_ws(anchored_text))`; sub-path granularity so a sibling symbol's edit is not a false alarm |
| Evidence grade ⊥ freshness (two axes) | EA-Graph | `claims.evidence_grade ∈ {proven, partial, unknown}` (a lattice) is separate from anchor `freshness ∈ {fresh, stale}` |
| "No model output enters at PROVEN" | EA-Graph | ADR-parsed / syntactically-extracted claims may be `proven`; every LLM-proposed claim enters `partial`; a boundary with no linkage is `unknown` |
| Three-state disposition (`unaffected` / `affected` / `unprovable`) | EA-Graph | Computed per claim from its anchors' freshness + whether replacement content is available; `unprovable` is terminal, not "low confidence" |
| Refusal + rebuild obligation when a stale fact is on the path | EA-Graph | `verify: "strict"` mode; default for `inspect_change_against_decisions` |
| Anchor completeness / downward closure | EA-Graph | Extraction records the artifact **read-set**; a claim is `complete` iff every read artifact is covered by an anchor |
| Disposition separate from artifact retention (DISP) | EA-Graph | `unprovable` never triggers `retract`; last verified anchor text + reason are retained |
| Per-commit view manifest: `commit, lag, status, capabilities` | CodeNib | `index_lanes` table + a **lane manifest** distinct from the per-response **view manifest** |
| `lag` measured in commit distance, not wall-clock | CodeNib | `lag_commits = rev-list --count <lane_commit>..HEAD` |
| View failure isolation + capability gating | CodeNib | Manifest declares which query modes are answerable; a tool requiring a failed lane errors explicitly instead of returning silent partial results |
| Edit classification `deleted/affected/shifted/unchanged/added` + location rebase | CodeNib | The re-verification algorithm; `shifted` anchors auto-rebase by line delta |
| Content-addressed reuse of unchanged units | CodeNib | Re-verification batches by file content hash; a file unchanged since its last verification at the same commit skips all its anchors |
| Offline output-equality check off the hot path | CodeNib | `verify_index_integrity` compares `sync_incremental` output to a full re-sync in CI; qualifies NFR-004 |
| Ground truth "known by construction" (synthetic worlds) | EA-Graph | Drift test fixtures are constructed, because real repos leak via git history and training data |

## Goals

1. **Claims** — decompose each decision/constraint/lead into individually
   verifiable assertions, each carrying an **evidence grade**.
2. **Content-hashed anchors** — every claim cites exact source spans by a
   canonical `(source_kind, source_uri, subpath)` identity plus a hash of the
   normalized span content; identity resolves aliases to the leaf definition.
3. **Per-evidence freshness** — each anchor is `fresh` or `stale` against a
   named git ref; verification is append-only and never mutates the anchor.
4. **Per-claim disposition** — `unaffected` / `affected` / `unprovable`, derived
   from anchor freshness + replacement availability.
5. **Per-view freshness manifest** — every retrieval response carries a
   `freshness` block scoped to *that response's* claims; a **lane manifest**
   describes index-lane freshness repo-wide.
6. **Refusal** — in strict mode a response whose claims are stale is refused
   with a structured rebuild obligation instead of a possibly-wrong answer.

## Non-goals

- No auto-repair beyond recording a rebased/relocated locator.
- No semantic re-anchoring. Relocation is exact hash + fuzzy text only.
- No new provider dependency. Verification uses `git2` + file reads.
- Claims do not replace decisions; they hang off existing records.
- `unprovable` never deletes or retracts anything.

---

## Core model

### Two independent axes

```
evidence grade  (grounds, set at ingest, promotes only on re-extraction)
    unknown  <  partial  <  proven
    proven   : deterministic ADR parse, tree-sitter extraction, hash-verified capture
    partial  : every LLM-proposed claim; keyword-overlap link
    unknown  : boundary crossing with no linkage evidence

freshness       (time, recomputed on demand against a git ref)
    fresh : content hash of the anchored span matches at the resolved locator
    stale : it does not
```

A claim can be `proven` **and** `stale` (a correctly-parsed ADR constraint whose
governed function was since deleted) — that combination is precisely what the
current single `confidence` float cannot express.

### Canonical anchor identity

```
anchor identity = (source_kind, source_uri, subpath)

source_kind : adr | episode | commit | pr | source_file | symbol
source_uri  : repo-relative path | ADR id | commit SHA
subpath     : symbol → leaf qualified name (aliases resolved)
              adr    → section name ("Decision", "Consequences")
              file   → {"lines":[s,e]} or {"chars":[a,b]}
              commit → whole message (subpath = "")
```

Alias resolution runs **before** hashing: a `pub use` re-export, a registry
binding, or an `entity_nodes` canonical name all resolve to the same leaf
identity, so a claim anchored to `OrderService::apply` is not falsely marked
stale when `orders::service` is re-exported under a new path.

### Three-state disposition

Computed per claim at manifest-assembly time:

```
unaffected : every anchor is fresh
affected   : ≥1 anchor is stale AND replacement content is available
             (shifted → rebased; edited → new span located; moved → relocated)
unprovable : ≥1 anchor is stale AND no replacement content is available
             (symbol deleted and not relocatable; commit squashed out;
              file present but anchored text gone with no fuzzy match)
```

`unprovable` is terminal for that verification — it is *not* a low confidence
score and it does not lower the claim's `evidence_grade`. The claim, its last
`anchored_text`, and the reason are retained.

---

## Requirements (new anchors)

REQ-053: Weaver must decompose recorded decisions, constraints, and observed
leads into individually addressable claims, each with an evidence grade.

Acceptance:
- `sync_adrs_from_git` and `record_decision_episode` populate `claims` for every
  decision and constraint they store; `synthesize_adr_leads` populates one
  `observation` claim per lead.
- ADR-parsed and tree-sitter-derived claims may be graded `proven`; every
  LLM-proposed claim is graded `partial`; grade never enters `proven` from model
  output without deterministic re-extraction.
- Retracting a subject closes its claims (`valid_to`); anchors and verifications
  are immutable and left as history.
- A claim with zero anchors is valid and reported `unprovable` with reason
  `no anchors`.

REQ-054: Weaver must cite exact source spans for each claim under a canonical
alias-resolved identity, hashed at span granularity.

Acceptance:
- Every ADR-sync claim cites the ADR section text it was extracted from; every
  episode claim cites the char range of the episode content the extractor used.
- Every ADR file-mention that resolves to an indexed symbol adds a `symbol`
  anchor whose `subpath` is the leaf qualified name.
- Anchors store `anchored_text`, `content_hash` (sha256 of whitespace-normalized
  span), `context_hash` (enclosing ±8 lines), the structured `locator`,
  `source_kind`, `source_uri`, `subpath`.
- Anchors are immutable; a changed citation is a new anchor.

REQ-055: Weaver must record the artifact read-set of each extraction so claim
anchor-completeness is checkable, not assumed.

Acceptance:
- Extraction records every artifact identity it read while forming a claim.
- A claim is `complete` iff every read identity is covered by one of its anchors
  (under container/entry subsumption).
- Incomplete claims are flagged in the manifest with the uncovered identities.

REQ-056: Weaver must verify each anchor against a named repository ref and record
the result append-only.

Acceptance:
- The verifier resolves each anchor to current text at a git ref (default
  `HEAD`), classifies the edit (`unchanged / shifted / affected / deleted`), sets
  `fresh` or `stale`, and appends an `evidence_verifications` row.
- `shifted` rebases the locator by line delta and stays `fresh` only if the span
  hash still matches; otherwise it is `stale` with a relocated locator when a
  fuzzy match (token overlap ≥ 0.6) exists.
- Verification never mutates the anchor or the claim.
- Re-verifying at a ref whose resolved commit already has an identical latest
  row for the anchor is a no-op.
- Files whose content hash is unchanged since the anchor's last verification at
  the same commit skip re-hashing.

REQ-057: Every retrieval tool that returns decisions must attach a per-view
freshness manifest, and expose a lane manifest.

Acceptance:
- `query`, `find_decisions_for_code`, `focused_file_brief`, `impact_of`,
  `inspect_change_against_decisions`, `get_architecture`, `trace_symbol_history`
  include a `freshness` object covering exactly the claims of the decisions in
  that response.
- The manifest reports the evaluated ref, counts by disposition, `stale_claims`
  detail, incomplete-claim detail, and the lane manifest snapshot it relied on.
- `verify` parameter selects `cached` (default) / `fresh` / `skip` / `strict`.
- `index_status` returns a lane manifest: per lane
  `{ last_ingested_commit, lag_commits, status, capabilities }`.
- A tool whose retrieval mode requires a failed or absent lane errors explicitly
  (names the lane and the rebuild command) rather than returning partial results.

REQ-058: In strict mode, a response whose claims are stale is refused.

Acceptance:
- `verify: "strict"` (default for `inspect_change_against_decisions`): if any
  claim on the returned/traversed path is `affected` or `unprovable`, the tool
  returns a `refused` result with a `rebuild_obligation` listing the drifted
  anchor ids and the exact ingest command(s) to run.
- Non-strict modes never refuse; they annotate.

---

## Data model

Migrations are append-only. `evidence_spans` is unused; a later migration (0019)
drops it once `claims` + `evidence_anchors` are populated by ADR sync and episode
ingestion.

### `migrations/0015_claims.sql`

```sql
CREATE TABLE IF NOT EXISTS claims (
    id             TEXT PRIMARY KEY NOT NULL,
    repo_id        TEXT NOT NULL REFERENCES repositories(id),
    kind           TEXT NOT NULL,   -- decision | constraint | observation | link
    subject_type   TEXT NOT NULL,   -- decision | constraint | decision_code_link | adr_lead
    subject_id     TEXT NOT NULL,
    text           TEXT NOT NULL,   -- normalized assertion
    polarity       TEXT,            -- must | must_not | null  (constraint claims)
    evidence_grade TEXT NOT NULL,   -- proven | partial | unknown
    read_set       TEXT NOT NULL DEFAULT '[]',  -- JSON [(source_kind, source_uri, subpath)]
    valid_from     TEXT NOT NULL,
    valid_to       TEXT,
    ingested_at    TEXT NOT NULL,
    source_time    TEXT,
    confidence     REAL NOT NULL DEFAULT 1.0    -- secondary continuous signal, kept
);
CREATE INDEX IF NOT EXISTS idx_claims_repo    ON claims(repo_id);
CREATE INDEX IF NOT EXISTS idx_claims_subject ON claims(subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_claims_valid_to ON claims(valid_to);
```

`polarity` feeds `check_consistency`'s contradictory-constraint detector, which
moves from ad-hoc text scanning to a claim join.

### `migrations/0016_evidence_anchors.sql`

```sql
CREATE TABLE IF NOT EXISTS evidence_anchors (
    id            TEXT PRIMARY KEY NOT NULL,
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    claim_id      TEXT NOT NULL REFERENCES claims(id),
    source_kind   TEXT NOT NULL,   -- adr | episode | commit | pr | source_file | symbol
    source_uri    TEXT NOT NULL,
    subpath       TEXT NOT NULL DEFAULT '',   -- leaf qualified name | section | "" 
    locator       TEXT NOT NULL,   -- JSON: {"lines":[s,e]} | {"symbol_qn":"..."} | {"section":"Decision"} | {"chars":[a,b]}
    anchored_text TEXT NOT NULL,
    content_hash  TEXT NOT NULL,   -- sha256(normalize_ws(anchored_text))
    context_hash  TEXT,            -- sha256(normalize_ws(enclosing +/- 8 lines))
    alias_of      TEXT,            -- pre-resolution identity string, if it differed
    ingested_at   TEXT NOT NULL,
    source_time   TEXT,
    UNIQUE(claim_id, source_kind, source_uri, subpath, content_hash)
);
CREATE INDEX IF NOT EXISTS idx_anchors_claim ON evidence_anchors(claim_id);
CREATE INDEX IF NOT EXISTS idx_anchors_uri   ON evidence_anchors(repo_id, source_uri);
CREATE INDEX IF NOT EXISTS idx_anchors_chash ON evidence_anchors(content_hash);

CREATE TABLE IF NOT EXISTS evidence_verifications (
    id             TEXT PRIMARY KEY NOT NULL,
    anchor_id      TEXT NOT NULL REFERENCES evidence_anchors(id),
    checked_at     TEXT NOT NULL,
    repo_ref       TEXT NOT NULL,   -- ref name as requested
    repo_commit    TEXT NOT NULL,   -- resolved SHA (the real cache key)
    edit_class     TEXT NOT NULL,   -- unchanged | shifted | affected | deleted
    freshness      TEXT NOT NULL,   -- fresh | stale
    observed_hash  TEXT,
    relocated_locator TEXT,         -- JSON, present when shifted/affected relocated
    similarity     REAL,            -- token overlap for affected-relocated
    detail         TEXT
);
CREATE INDEX IF NOT EXISTS idx_verif_anchor ON evidence_verifications(anchor_id);
CREATE INDEX IF NOT EXISTS idx_verif_commit ON evidence_verifications(anchor_id, repo_commit);
```

### `migrations/0017_index_lanes.sql`

```sql
CREATE TABLE IF NOT EXISTS index_lanes (
    repo_id             TEXT NOT NULL REFERENCES repositories(id),
    lane                TEXT NOT NULL,   -- adr | symbol | commit | embedding | community | route
    last_ingested_commit TEXT,
    last_ingested_at    TEXT NOT NULL,
    status              TEXT NOT NULL,   -- ok | failed | absent
    detail              TEXT,
    PRIMARY KEY (repo_id, lane)
);
```

`lag_commits` is derived at read time (`git rev-list --count <commit>..HEAD`),
not stored — it is only meaningful relative to current HEAD.

### `migrations/0018_freshness_manifests.sql`

```sql
CREATE TABLE IF NOT EXISTS freshness_manifests (
    id            TEXT PRIMARY KEY NOT NULL,
    repo_id       TEXT NOT NULL REFERENCES repositories(id),
    tool          TEXT NOT NULL,
    view_hash     TEXT NOT NULL,   -- sha256(tool + canonical input args)
    repo_commit   TEXT NOT NULL,
    evaluated_at  TEXT NOT NULL,
    payload       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_manifest_lookup
    ON freshness_manifests(repo_id, tool, view_hash, repo_commit);
```

Manifests are a cache + audit trail, never source of truth. A new ingest does not
delete them; lookups filter on `repo_commit`.

---

## Domain types (`src/domain/entities.rs`)

```rust
pub enum ClaimKind    { Decision, Constraint, Observation, Link }
pub enum EvidenceGrade { Unknown, Partial, Proven }   // Ord: Unknown < Partial < Proven
pub enum Polarity     { Must, MustNot }
pub enum AnchorSource { Adr, Episode, Commit, Pr, SourceFile, Symbol }
pub enum Freshness    { Fresh, Stale }
pub enum EditClass    { Unchanged, Shifted, Affected, Deleted }
pub enum Disposition  { Unaffected, Affected, Unprovable }

pub enum Locator {                              // serde-tagged
    Lines { start: u32, end: u32 },
    SymbolQn { qn: String },
    Section { name: String },
    Chars { start: usize, end: usize },
}

pub struct Claim {
    pub id: Uuid, pub repo_id: Uuid,
    pub kind: ClaimKind,
    pub subject_type: String, pub subject_id: Uuid,
    pub text: String,
    pub polarity: Option<Polarity>,
    pub evidence_grade: EvidenceGrade,
    pub read_set: Vec<AnchorIdentity>,
    pub valid_from: String, pub valid_to: Option<String>,
    pub ingested_at: String, pub source_time: Option<String>,
    pub confidence: f64,
}

pub struct AnchorIdentity { pub source_kind: AnchorSource, pub source_uri: String, pub subpath: String }

pub struct EvidenceAnchor {
    pub id: Uuid, pub repo_id: Uuid, pub claim_id: Uuid,
    pub identity: AnchorIdentity,
    pub locator: Locator,
    pub anchored_text: String,
    pub content_hash: String, pub context_hash: Option<String>,
    pub alias_of: Option<String>,
    pub ingested_at: String, pub source_time: Option<String>,
}

pub struct AnchorVerification {
    pub anchor_id: Uuid, pub checked_at: String,
    pub repo_ref: String, pub repo_commit: String,
    pub edit_class: EditClass, pub freshness: Freshness,
    pub observed_hash: Option<String>,
    pub relocated_locator: Option<Locator>,
    pub similarity: Option<f64>,
    pub detail: Option<String>,
}

pub struct StaleClaim {
    pub claim_id: Uuid, pub subject_type: String, pub subject_id: Uuid,
    pub decision_id: Option<Uuid>, pub adr_id: Option<String>,
    pub text: String,
    pub disposition: Disposition,
    pub anchors: Vec<StaleAnchorDetail>,
}

pub struct FreshnessManifest {
    pub evaluated_at: String,
    pub repo_ref: String, pub repo_commit: String,
    pub anchors_total: usize,
    pub by_disposition: BTreeMap<String, usize>,   // unaffected|affected|unprovable
    pub stale_claims: Vec<StaleClaim>,
    pub incomplete_claims: Vec<IncompleteClaim>,
    pub lanes: Vec<LaneStatus>,                    // snapshot of the lane manifest
    pub warnings: Vec<String>,
}

pub struct LaneStatus {
    pub lane: String, pub last_ingested_commit: Option<String>,
    pub lag_commits: Option<u32>, pub status: String,
    pub capabilities: Vec<String>,   // e.g. "semantic_query", "graph_expansion"
}

pub enum VerifyMode { Cached, Fresh, Skip, Strict }
```

`ArchResponse` gains `pub freshness: Option<FreshnessManifest>` (omitted from
JSON when `None`) and, for strict refusals, a `pub refused: Option<RebuildObligation>`.

```rust
pub struct RebuildObligation {
    pub reason: String,
    pub drifted_anchors: Vec<Uuid>,
    pub commands: Vec<String>,   // "sync_incremental { repo_path, since: '<commit>' }"
}
```

---

## Components (spec + role, per AGENTS.md)

| Component | Spec | Role |
|---|---|---|
| `src/domain/claims.rs` | REQ-053 | Split a decision/constraint/lead into normalized `Claim` values, assign `evidence_grade` from the extraction method, detect constraint polarity. Pure. **Strategy/Formatter.** |
| `src/domain/anchors.rs` | REQ-054 | Build an `EvidenceAnchor` from a captured span: resolve identity aliases to the leaf, normalize whitespace, compute `content_hash` / `context_hash`, encode the `locator`. Pure. **Formatter.** |
| `src/domain/completeness.rs` | REQ-055 | Given a claim's `read_set` and its anchors, decide `complete` / list uncovered identities under subsumption. Pure. **Policy.** |
| `src/tools/verify_evidence.rs` | REQ-056 | Resolve an anchor to current text at a commit, classify the edit, set freshness, append a verification. **Verifier.** |
| `src/tools/freshness.rs` (extend) | REQ-057/058 | Assemble the per-view `FreshnessManifest` for a claim-id set: get-or-run verifications, compute per-claim `Disposition`, fold in the lane manifest, and in strict mode build the `RebuildObligation`. Keeps `stale_index_warning` as one lane input. **Assembler.** |
| `src/tools/index_manifest.rs` | REQ-057 | Read `index_lanes`, derive `lag_commits`, derive per-lane capabilities. **Reporter.** Backs `index_status`. |
| `src/tools/verify_index_integrity.rs` | NFR-004 | Off-hot-path: run a full re-sync into a scratch db and compare the declared-fact projection to the incrementally-maintained store. **Oracle.** CI / on-demand only. |
| `src/storage/sqlite/claims.rs` | REQ-053..057 | Persist and query `claims`, `evidence_anchors`, `evidence_verifications`, `index_lanes`, `freshness_manifests`. |

No new provider trait, no abstraction over `git2`.

---

## Verification algorithm (`verify_evidence`)

Input: `EvidenceAnchor`, target ref → resolved `repo_commit`.

**Batch guard (CodeNib content-addressed reuse).** Group anchors by
`source_uri`. For each file, hash its blob at `repo_commit`. If that hash equals
the file hash recorded at the anchor's most recent verification for the same
`repo_commit`, copy the previous verdicts forward — no re-hash.

**Per anchor:**

1. **Resolve current text.**
   - `symbol`: re-resolve `subpath` (leaf qn) through the symbol index at the
     nearest ingest ≤ commit; alias chains re-resolved. Fall back to the file
     blob + recorded line window.
   - `source_file`: slice the blob at the locator.
   - `adr`: load the ADR file, slice the named section.
   - `commit`: SHA is an ancestor of `repo_commit`? present → `unchanged/fresh`;
     absent → `deleted/stale`, detail `"commit not in history of <ref>"`.
   - `episode` / `pr`: immutable → `unchanged/fresh` (or `deleted/stale` if the
     stored content was retracted).

2. **Classify + hash.**
   - Text byte-identical at the recorded locator → `unchanged`, `fresh`.
   - Normalized-hash match but line numbers moved → `shifted`, `fresh`,
     `relocated_locator` = rebased range.
   - No hash match at locator, but `normalize_ws(anchored_text)` found elsewhere
     in the file (or a window whose `context_hash` matches) → `shifted`, `fresh`,
     relocated.
   - Best-window token-set overlap ≥ 0.6 → `affected`, `stale`, `similarity`
     recorded, `relocated_locator` set.
   - Otherwise → `deleted`, `stale`.

3. Append `evidence_verifications` unless an identical latest row exists for
   `(anchor_id, repo_commit)`.

Constants (`0.6`, `±8` lines) are module constants, not config — YAGNI.

## Disposition + refusal (`freshness.rs`)

Per claim, over its open anchors' latest verifications at `repo_commit`:

```
all fresh                              -> unaffected
any stale, all stale anchors relocated -> affected      (cite the new locator)
any stale anchor not relocated,
    OR claim has zero anchors          -> unprovable
```

`build_manifest(store, repo_id, repo_commit, claim_ids, mode)`:

1. `mode = skip` → return `None`; caller adds a warning.
2. Fetch open anchors for `claim_ids`; run the batch guard.
3. `cached` → use verifications whose `repo_commit` equals the current one;
   verify the rest. `fresh` / `strict` → verify all.
4. Compute each claim's `Disposition`; run `completeness` for each.
5. Aggregate `by_disposition`; build `stale_claims` and `incomplete_claims`.
6. Attach the lane manifest snapshot; fold `stale_index_warning` into `warnings`
   when the ADR or symbol lane lags HEAD.
7. Persist to `freshness_manifests` under `view_hash`.
8. `strict` and any claim `affected|unprovable` on the path → the tool returns
   `refused` with a `RebuildObligation` naming the anchors and the commands
   (`sync_incremental` for shifted/affected source, `sync_adrs_from_git` for ADR
   drift, `sync_commits_from_git` for missing commits).

The retrieval tool derives `claim_ids` from the decisions it is already about to
return — the manifest is scoped to the view, never the whole repo.

## Lane capabilities (CodeNib)

`index_manifest` derives, per lane:

| Lane | `status` drivers | Capabilities it enables |
|---|---|---|
| `adr` | last `sync_adrs_from_git` vs. changed ADR files | `governance_lookup`, `consistency_check` |
| `symbol` | last `ingest_symbols` vs. changed source files | `symbol_lookup`, `call_path`, `anchor_verification` |
| `commit` | last `sync_commits_from_git` vs. `git HEAD` | `commit_evidence`, `stale_decision_activity` |
| `embedding` | `embed_all` coverage ratio | `semantic_query` |
| `community` | community pass completion | `architecture_communities` |
| `route` | route pass completion | `route_lookup` |

A tool whose selected retrieval mode needs a capability the lane manifest does
not offer **errors explicitly** — e.g. `query` with `verify: strict` on a repo
whose `symbol` lane is `absent` returns `"cannot verify anchors: symbol lane not
indexed — run ingest_symbols"`, rather than a manifest full of `unverifiable`.
The daemon surfaces the lane manifest once at session `initialize`.

---

## Response shape

```json
{
  "answer": "...",
  "decisions": [ { "id": "d77a...", "adr_id": "ADR-0042", "...": "..." } ],
  "constraints": [ "..." ],
  "freshness": {
    "evaluated_at": "2026-09-01T18:22:04Z",
    "repo_ref": "HEAD",
    "repo_commit": "9f1c2ab",
    "anchors_total": 7,
    "by_disposition": { "unaffected": 5, "affected": 1, "unprovable": 1 },
    "stale_claims": [
      {
        "claim_id": "b2e1...",
        "subject_type": "constraint",
        "subject_id": "c009...",
        "decision_id": "d77a...",
        "adr_id": "ADR-0042",
        "text": "Order state must never be mutated in place",
        "disposition": "unprovable",
        "anchors": [
          {
            "anchor_id": "a34f...",
            "identity": { "source_kind": "symbol", "source_uri": "src/orders/service.rs", "subpath": "OrderService::apply" },
            "edit_class": "deleted",
            "freshness": "stale",
            "detail": "anchored text not found at 9f1c2ab; OrderService::apply absent from symbol index; no fuzzy match ≥ 0.6"
          }
        ]
      }
    ],
    "incomplete_claims": [
      { "claim_id": "f110...", "uncovered": [ { "source_kind": "source_file", "source_uri": "src/orders/events.rs", "subpath": "" } ] }
    ],
    "lanes": [
      { "lane": "adr", "last_ingested_commit": "9f1c2ab", "lag_commits": 0, "status": "ok", "capabilities": ["governance_lookup","consistency_check"] },
      { "lane": "symbol", "last_ingested_commit": "3ab77e1", "lag_commits": 6, "status": "ok", "capabilities": ["symbol_lookup","call_path","anchor_verification"] },
      { "lane": "embedding", "last_ingested_commit": null, "lag_commits": null, "status": "absent", "capabilities": [] }
    ],
    "warnings": ["symbol lane lags HEAD by 6 commits — anchor verification used the last indexed tree"]
  }
}
```

Strict refusal:

```json
{
  "refused": {
    "reason": "1 claim on the answer path is unprovable, 1 is affected",
    "drifted_anchors": ["a34f...", "9c02..."],
    "commands": [
      "sync_incremental { \"repo_path\": \"...\", \"since\": \"3ab77e1\" }",
      "sync_adrs_from_git { \"repo_path\": \"...\", \"adr_glob\": \"docs/adr/*.md\" }"
    ]
  },
  "freshness": { "...": "manifest still included for triage" }
}
```

**Agent contract.** A decision is fully authoritative only when none of its
claims appear in `stale_claims` and none in `incomplete_claims`. `affected` →
re-read at the cited new locator. `unprovable` → the decision may still be right
but Weaver cannot show it; re-ingest or treat as unverified. `evidence_grade`
is orthogonal: a `partial` claim was never strongly grounded even when `fresh`.

---

## Tool surface changes

- **New param** on the seven retrieval tools: `verify`
  (`"cached"` default; `"fresh"`, `"skip"`, `"strict"`).
  `inspect_change_against_decisions` defaults to `"strict"`.
- **New tool `verify_claims`** — given an ADR id, decision id, or file path,
  return full anchor + verification + disposition + completeness detail at a ref.
  Debugging counterpart to the inline manifest, as `explain_answer` is to `query`.
- **New tool `verify_index_integrity`** — off-hot-path oracle (NFR-004): full
  re-sync into a scratch db, compare the declared-fact projection
  (decisions, constraints, links, edges, symbols with source anchors) to the
  live store; report divergences. Not registered for agent hot paths; intended
  for CI and manual audit.
- `index_status` → returns the lane manifest (`last_ingested_commit`,
  `lag_commits`, `status`, `capabilities`) in addition to today's counts.
- `check_consistency` → contradictory-constraint detection reads
  `claims.polarity` instead of re-scanning constraint text.
- `find_stale_decisions` → gains signal `drifted_evidence` (confidence 0.6 for
  `affected`, 0.8 for `unprovable`) sourced from anchor verifications.
- `retract` → closing a decision/constraint/episode sets `valid_to` on its
  claims. Anchors and verifications are immutable history. `unprovable` never
  triggers `retract` (DISP separation).

## Ingestion changes

| Tool | New behavior |
|---|---|
| `sync_adrs_from_git` | Per decision: one `decision` claim, `evidence_grade = proven` (deterministic parse), anchored to the ADR "Decision" section. Per constraint: one `constraint` claim with polarity, `proven`, anchored to the obligation sentence. Per file mention resolving to an indexed symbol: a `symbol` anchor (leaf qn) on the decision claim. `read_set` = the ADR sections + mentioned files. |
| `record_decision_episode` | Per stored decision/constraint: one claim, `evidence_grade = partial` (LLM extraction) or `proven` if the caller supplied structured text verbatim, anchored to the cited char range of `content`. On dedup-merge, new anchors attach to the surviving decision's existing claim. `read_set` = episode content span(s). |
| `synthesize_adr_leads` | One `observation` claim per lead, `evidence_grade = partial`, anchored to the symbol spans named in `affected_files`. `read_set` = those files + injected context files. |
| `sync_commits_from_git` | On a commit→decision link, add a `commit` anchor to the decision claim (SHA locator, `evidence_grade` unchanged on the claim; the link claim itself is `partial` for keyword-overlap, `proven` for explicit ADR-ID match). |
| `ingest_symbols` | After the content-hash pass, opportunistically re-verify anchors for changed files at HEAD so the next `cached` manifest is warm. Update `index_lanes` for the `symbol` lane. |

All anchor writes are idempotent on the `UNIQUE(claim_id, source_kind,
source_uri, subpath, content_hash)` key.

## Offline integrity oracle (`verify_index_integrity`)

Adapted from CodeNib's `ℱ(Ĝ) = ℱ(G)` check. Not on any serving path.

1. Copy the repo's rows to a scratch SQLite db.
2. Run a full `sync_adrs_from_git` + `ingest_symbols` + `sync_commits_from_git`
   into a second scratch db from the same working tree.
3. Compare the **declared-fact projection**: the tagged multiset of
   `(decision.text, adr_id)`, `(constraint.text, polarity)`,
   `(decision_code_link: decision, file, symbol)`, temporal edges
   `(type, source, target)`, and symbols `(qn, file, span)`.
4. Report additions/removals. A non-empty diff means `sync_incremental` (or a
   claim/anchor population path) has drifted from a clean build.

CI job: run on PRs that touch ingestion or storage.

## Performance

- Verification cost is bounded by anchors-per-view (typically < 20) and, with the
  batch guard, by *changed* files among them.
- `cached` mode makes repeat identical views O(1) after the first.
- `verify_index_integrity` is O(full re-index) and deliberately excluded from
  agent-facing tool routing.

## Testing

Drift fixtures are **constructed** (EA-Graph's rationale: real repos leak ground
truth through git history and model training data). `tests/fixtures/drift/` holds
a base tree plus scripted edits, one per outcome:

- `domain::claims` — decision/constraint split; polarity (`must` / `must not` /
  `never` / `prohibit`); grade assignment (ADR parse → `proven`, mock-LLM
  episode → `partial`).
- `domain::anchors` — whitespace-normalization stability; hash determinism;
  alias resolution (`pub use` re-export resolves to the same identity); locator
  round-trip.
- `domain::completeness` — read artifact not anchored → listed; subsumed entry
  covered by its container anchor → complete.
- `verify_evidence` — one case per `EditClass`: `unchanged` (untouched),
  `shifted` (function pushed down by an added import — stays `fresh`),
  `affected` (renamed local var — `stale`, relocated, similarity recorded),
  `deleted` (function removed — `stale`, `unprovable`), commit `deleted`
  (history rewritten). Batch guard: two anchors in one unchanged file → one blob
  hash, verdicts copied.
- `freshness::build_manifest` — `cached` hit vs. re-verify on commit change;
  zero-anchor claim → `unprovable`; scoping (a decision absent from the response
  contributes no anchors); strict refusal builds the correct `RebuildObligation`.
- `index_manifest` — `lag_commits` math; `embedding` lane absent → no
  `semantic_query` capability; `query` strict on absent `symbol` lane → explicit
  error.
- Integration (`tests/fixture_repo.rs`): sync → ingest → delete a governed
  symbol → `query` returns that decision with the constraint claim
  `unprovable`, `by_disposition.unprovable == 1`; `verify: strict` refuses with
  the symbol file in `commands`.

## Rollout

**Status: complete (all 9 steps landed).** Migrations 0015–0019, domain modules
(`anchors`, `claims`, `completeness`), storage (`src/storage/sqlite/claims.rs`),
tools (`verify_claims`, `verify_index_integrity`), `freshness.rs` manifest
assembly, `verify` params on `query` / `find_decisions_for_code` /
`inspect_change_against_decisions` (strict-default), lane tracking in
`index_status`, and the CI integrity step (`tests/index_integrity.rs`) are all in
place and covered by tests. One deviation from the plan below: the daemon
session-init capability surface (step 5) was folded into `index_status.lanes`
rather than a separate `index_manifest` tool — KISS, one surface for lane state.

1. 0015–0018 migrations + domain types + storage. No behavior change.
2. Claim + anchor + `read_set` population in `sync_adrs_from_git` and
   `record_decision_episode`; `evidence_grade` assignment.
3. `verify_evidence` verifier + `verify_claims` tool.
4. `freshness.rs` manifest assembly + disposition + `verify` param on retrieval
   tools (`cached`/`fresh`/`skip`).
5. `index_lanes` population + `index_manifest` + capability gating + daemon
   session-init surface.
6. `verify: strict` + `RebuildObligation`; default it for
   `inspect_change_against_decisions`.
7. `check_consistency` / `find_stale_decisions` / `retract` wiring;
   `synthesize_adr_leads` + `sync_commits_from_git` anchor population.
8. `verify_index_integrity` + CI job.
9. Migration 0019 drops `evidence_spans`.

Docs updated per step: `README.md` (Phase 4 list, tool docs, response shape),
`TECHNICAL_BEHAVIOR.md` (per-tool sections, migration list, close the
`evidence_spans` gap), `ENGINEERING_REQUIREMENTS.md` (REQ-053..058, component
table, remove the two Open Requirements).

## Open questions

- **Strict default scope.** Only `inspect_change_against_decisions`, or also
  `query` when `graph_depth > 0`? Leaning: opt-in for `query`, strict-default
  only for the change-inspection tool, since that is the one whose wrong answer
  causes a bad merge.
- **Episode char-range anchors** require the LLM extractor to return offsets.
  Fallback when it does not: anchor the whole episode content (`subpath = ""`),
  grade `partial`, mark the claim `incomplete`.
- **Alias resolution depth.** Follow `pub use` chains within the repo only; a
  re-export from an external crate terminates at the crate boundary as
  `unknown`-grade with no anchor.
- **Lane `lag` for non-git repos.** `lag_commits` is `null`; fall back to the
  existing timestamp comparison in `warnings`.
```
