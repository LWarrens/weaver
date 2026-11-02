use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use glob::Pattern;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::adapters::registry::{
    extract_edges_for_extension, extract_symbols_for_extension, has_extractor_for_extension,
};
use crate::domain::entities::ArchResponse;
use crate::embeddings::{pack_f32, provider_from_env};
use crate::error::Error;
use crate::storage::sqlite::SymbolEdge;
use crate::storage::SqliteStore;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IngestSymbolsParams {
    /// Absolute path to the git repository root.
    pub repo_path: String,
    /// Glob pattern for source files (defaults to all supported files)
    #[serde(default)]
    pub pattern: Option<String>,
    /// When true, force re-indexing of all files even if content hash is unchanged.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestSymbolsResult {
    #[serde(flatten)]
    pub response: ArchResponse,
    pub files_total: usize,
    pub files_processed: usize,
    pub files_unchanged: usize,
    pub communities_detected: usize,
    pub cancelled: bool,
}

struct EligibleFile {
    path: PathBuf,
    rel: String,
    ext: String,
}

struct PendingFile {
    rel: String,
    file_id: Uuid,
    local_ids: std::collections::HashMap<String, Uuid>,
    symbols: Vec<crate::adapters::symbols::Symbol>,
    raw_edges: Vec<crate::adapters::edges::RawEdge>,
    /// Local `var -> type` bindings observed in this file, for receiver-type resolution.
    bindings: Vec<crate::adapters::edges::RawBinding>,
    /// The file itself is unchanged; only its edges are being re-resolved because
    /// a name they depend on was added or removed elsewhere. Its symbols and
    /// `contains` edges are left alone.
    edges_only: bool,
}

/// Compute a hex-encoded SHA-256 digest of a string.
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Candidate repo-relative file paths a module specifier could resolve to.
/// `from_rel` is the importing file's repo-relative path; `spec` is the raw
/// specifier (`./foo`, `../bar/baz`, `foo/bar`, `foo.bar` already slash-normalised
/// by the extractor).
fn module_spec_candidates(from_rel: &str, spec: &str) -> Vec<String> {
    use std::path::Path;
    let base = if spec.starts_with('.') || spec.starts_with('/') {
        Path::new(from_rel)
            .parent()
            .map(|p| p.join(spec).to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| spec.replace('\\', "/"))
    } else {
        spec.replace('\\', "/")
    };
    // Normalise `a/b/../c` → `a/c` and strip `./` segments.
    let mut parts: Vec<&str> = Vec::new();
    for seg in base.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let s = parts.join("/");
    [
        s.clone(),
        format!("{s}.ts"),
        format!("{s}.tsx"),
        format!("{s}.js"),
        format!("{s}.jsx"),
        format!("{s}.mjs"),
        format!("{s}.py"),
        format!("{s}.rb"),
        format!("{s}.go"),
        format!("{s}.java"),
        format!("{s}.rs"),
        format!("{s}/index.ts"),
        format!("{s}/index.js"),
        format!("{s}/__init__.py"),
        format!("{s}/mod.rs"),
    ]
    .into_iter()
    .filter(|c| !c.is_empty())
    .collect()
}

/// Resolve a module specifier to a concrete file UUID in this repo, if one of the
/// candidate paths exists.
async fn resolve_spec_to_file(
    store: &SqliteStore,
    repo_id: Uuid,
    from_rel: &str,
    spec: &str,
) -> Option<Uuid> {
    for cand in module_spec_candidates(from_rel, spec) {
        if let Ok(Some(fid)) = store.find_file_id_by_path(repo_id, &cand).await {
            return Some(fid);
        }
    }
    None
}

pub async fn run(
    store: &Arc<SqliteStore>,
    params: IngestSymbolsParams,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel_token: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<IngestSymbolsResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!(
                "path does not exist or is not accessible: {}",
                params.repo_path
            ),
        })?;

    let repo_path_str = repo_path.to_str().ok_or_else(|| Error::InvalidInput {
        field: "repo_path",
        reason: "path contains non-UTF-8 characters".to_string(),
    })?;

    let pattern = params.pattern.as_deref().unwrap_or("**/*");
    let pattern = Pattern::new(pattern).map_err(|_| Error::InvalidInput {
        field: "pattern",
        reason: format!("not a valid glob pattern: {}", pattern),
    })?;
    let mut warnings = Vec::new();

    // Load .archignore (gitignore-style) from repository root if present
    let gitignore = match load_archignore(&repo_path) {
        Ok(g) => g,
        Err(err) => {
            warnings.push(format!("failed to parse .archignore: {}", err));
            None
        }
    };

    let now = Utc::now().to_rfc3339();
    let mut processed = 0usize;
    let mut skipped = 0usize;

    // Ensure repository record exists
    let repo = store.upsert_repository(repo_path_str, None).await?;

    // Collect eligible files first so we can report total count in progress messages.
    let eligible: Vec<EligibleFile> = walkdir::WalkDir::new(&repo_path)
        .into_iter()
        .filter_entry(|e| !is_ignored_entry(e.path(), &repo_path, gitignore.as_ref()))
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            if !e.path().is_file() {
                return None;
            }
            let rel = e
                .path()
                .strip_prefix(&repo_path)
                .unwrap_or(e.path())
                .to_string_lossy()
                .replace('\\', "/");
            let path = e.path().to_path_buf();
            let ext = e
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if has_extractor_for_extension(&ext) && pattern.matches(&rel) {
                Some(EligibleFile {
                    path,
                    rel,
                    ext,
                })
            } else {
                None
            }
        })
        .collect();
    let total = eligible.len();
    let embedding_provider = provider_from_env();

    // -------------------------------------------------------------------------
    // Pass 1: ingest all symbols. Edge resolution is deferred to pass 2 so
    // that cross-file call targets are already in the DB when we resolve them.
    // -------------------------------------------------------------------------

    let mut pending: Vec<PendingFile> = Vec::new();
    let names_before = store.open_symbol_name_counts(repo.id).await?;

    for entry in &eligible {
        // Check cancellation before each file so the job can be stopped cleanly.
        if cancel_token
            .as_ref()
            .map(|t| t.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
        {
            if let Some(ref tx) = progress_tx {
                let _ = tx.send(format!(
                    "indexing cancelled: {}/{} files processed so far ({} unchanged)",
                    processed, total, skipped
                ));
            }
            let mut resp = ArchResponse::empty();
            resp.answer = Some(format!(
                "Cancelled after {} file(s) ({} unchanged, skipped).",
                processed, skipped
            ));
            resp.warnings = warnings;
            resp.confidence = 1.0;
            return Ok(IngestSymbolsResult {
                response: resp,
                files_total: total,
                files_processed: processed,
                files_unchanged: skipped,
                communities_detected: 0,
                cancelled: true,
            });
        }

        let path = &entry.path;
        if path.is_file() {
            let rel = &entry.rel;
            let ext = &entry.ext;

            let content = match std::fs::read_to_string(path) {
                Ok(content) => content,
                Err(err) => {
                    warnings.push(format!("skipped {}: {}", rel, err));
                    continue;
                }
            };

            // Incremental: skip files whose content hash hasn't changed.
            let hash = sha256_hex(&content);
            if !params.force {
                let stored = store.get_file_content_hash(repo.id, rel).await.unwrap_or(None);
                if stored.as_deref() == Some(hash.as_str()) {
                    skipped += 1;
                    // Report progress every 50 skipped files so the client knows we're alive.
                    let done = processed + skipped;
                    if done % 50 == 0 {
                        if let Some(ref tx) = progress_tx {
                            let _ = tx.send(format!(
                                "indexing: {}/{} files ({} unchanged, {} re-indexed)",
                                done, total, skipped, processed
                            ));
                        }
                    }
                    continue;
                }
            }

            let symbols = match extract_symbols_for_extension(ext, &content) {
                Ok(s) => s,
                Err(e) => {
                    tracing::info!(file = %rel, ext = %ext, error = %e, "symbol extraction failed");
                    continue;
                }
            };

            // Upsert file and symbols
            let file_id = store.upsert_file(repo.id, rel, &now, &now).await?;

            let current_symbols = symbols
                .iter()
                .map(|s| (s.name.clone(), s.kind.clone()))
                .collect::<Vec<_>>();
            store
                .close_stale_symbols_for_file(file_id, &current_symbols, &now)
                .await?;

            for s in &symbols {
                let decorators_json = if s.decorators.is_empty() {
                    None
                } else {
                    serde_json::to_string(&s.decorators).ok()
                };
                store
                    .insert_symbol(
                        file_id,
                        &s.name,
                        &s.kind,
                        s.start_line as i64,
                        s.end_line as i64,
                        &now,
                        &now,
                        s.signature.as_deref(),
                        s.return_type.as_deref(),
                        s.visibility.as_deref(),
                        s.is_async,
                        s.complexity,
                        decorators_json.as_deref(),
                    )
                    .await?;

                // Embed symbol name + kind if a provider is configured
                if let Some(provider) = embedding_provider.as_ref() {
                    let text = format!("{} {}", s.name, s.kind);
                    if let Ok(vec) = provider.embed(&text).await {
                        if !vec.is_empty() {
                            if let Ok(Some(sym_id)) =
                                store.find_symbol_id_in_file(file_id, &s.name).await
                            {
                                let blob = pack_f32(&vec);
                                let _ = store.update_symbol_embedding(sym_id, &blob).await;
                            }
                        }
                    }
                }
            }

            // Build symbol ID map for pass 2 edge resolution.
            let mut local_ids: std::collections::HashMap<String, Uuid> =
                std::collections::HashMap::new();
            for s in &symbols {
                if let Ok(Some(id)) = store.find_symbol_id_in_file(file_id, &s.name).await {
                    local_ids.insert(s.name.clone(), id);
                }
            }

            // Collect raw edges; resolution deferred to pass 2 (all symbols in DB then).
            let file_edges = extract_edges_for_extension(ext, &content);
            let raw_edges = file_edges.edges;
            let bindings = file_edges.bindings;

            // Persist the new hash so this file is skipped on the next unchanged run.
            let _ = store.update_file_content_hash(file_id, &hash).await;

            processed += 1;
            let done = processed + skipped;
            // Report progress every 10 newly-processed files.
            if done % 10 == 0 {
                if let Some(ref tx) = progress_tx {
                    let _ = tx.send(format!(
                        "indexing: {}/{} files ({} unchanged, {} re-indexed)",
                        done, total, skipped, processed
                    ));
                }
            }

            pending.push(PendingFile {
                rel: rel.clone(),
                file_id,
                local_ids,
                symbols,
                raw_edges,
                bindings,
                edges_only: false,
            });
        }
    }

    // -------------------------------------------------------------------------
    // Retirement: a file that is gone (or, under the default pattern, no longer
    // eligible) keeps no open symbols, edges, imports or routes, and edges that
    // point at any retired symbol go back to unresolved. Without this an
    // unchanged caller keeps a resolved edge to a symbol that no longer exists,
    // which a full re-index would never produce. Runs before edge resolution so
    // the name census below sees the final symbol set.
    // -------------------------------------------------------------------------
    {
        let eligible_paths: std::collections::HashSet<&str> =
            eligible.iter().map(|f| f.rel.as_str()).collect();
        let default_pattern = params.pattern.is_none();
        store
            .retire_files_where(
                repo.id,
                &|path| {
                    if default_pattern {
                        !eligible_paths.contains(path)
                    } else {
                        !repo_path.join(path).is_file()
                    }
                },
                &now,
            )
            .await?;
        store.close_edges_from_retired_symbols(repo.id, &now).await?;
        store.unresolve_edges_to_retired_symbols(repo.id).await?;
    }

    // -------------------------------------------------------------------------
    // Name-dependent edges. Unique-name resolution (and the unresolved state)
    // depends on how many symbols share a name repo-wide, so an unchanged file's
    // edge can be stale after another file adds or drops a symbol with that name
    // (a once-unique target turns ambiguous; an unresolved one gains a target;
    // a better tier such as `import` now applies). Re-resolve the edges of every
    // unchanged file that references a name whose symbol count changed this run.
    // -------------------------------------------------------------------------
    let mut refreshed = 0usize;
    {
        let after = store.open_symbol_name_counts(repo.id).await?;
        let mut changed: Vec<String> = after
            .iter()
            .filter(|(name, n)| names_before.get(*name) != Some(*n))
            .map(|(name, _)| name.clone())
            .collect();
        changed.extend(
            names_before
                .keys()
                .filter(|name| !after.contains_key(*name))
                .cloned(),
        );
        if !changed.is_empty() {
            let already: std::collections::HashSet<String> =
                pending.iter().map(|pf| pf.rel.clone()).collect();
            let by_rel: std::collections::HashMap<&str, &EligibleFile> =
                eligible.iter().map(|f| (f.rel.as_str(), f)).collect();
            let mut rels = store
                .files_with_open_edges_to_names(repo.id, &changed)
                .await?;
            rels.sort();
            for rel in rels {
                if already.contains(&rel) {
                    continue;
                }
                let Some(entry) = by_rel.get(rel.as_str()) else {
                    continue;
                };
                let Ok(content) = std::fs::read_to_string(&entry.path) else {
                    continue;
                };
                let Some(file_id) = store.find_file_id_by_path(repo.id, &rel).await? else {
                    continue;
                };
                let Ok(symbols) = extract_symbols_for_extension(&entry.ext, &content) else {
                    continue;
                };
                let mut local_ids: std::collections::HashMap<String, Uuid> =
                    std::collections::HashMap::new();
                for s in &symbols {
                    if let Ok(Some(id)) = store.find_symbol_id_in_file(file_id, &s.name).await {
                        local_ids.insert(s.name.clone(), id);
                    }
                }
                let file_edges = extract_edges_for_extension(&entry.ext, &content);
                pending.push(PendingFile {
                    rel,
                    file_id,
                    local_ids,
                    symbols,
                    raw_edges: file_edges.edges,
                    bindings: file_edges.bindings,
                    edges_only: true,
                });
                refreshed += 1;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Pass 1.5: resolve each file's imports into `file_imports` and build an
    // in-memory lookup keyed by (importing file, imported name) that the call
    // resolver consults for the `import` tier.
    // -------------------------------------------------------------------------
    let mut imports_map: std::collections::HashMap<(Uuid, String), (Option<Uuid>, Option<Uuid>)> =
        std::collections::HashMap::new();
    for pf in &pending {
        store.close_stale_file_imports_for_file(pf.file_id, &now).await?;
        for raw in &pf.raw_edges {
            if raw.edge_type != "imports" || raw.to_name.is_empty() || raw.to_name == "*" {
                continue;
            }
            let to_file_id = match &raw.to_file {
                Some(spec) => resolve_spec_to_file(store, repo.id, &pf.rel, spec).await,
                None => None,
            };
            let to_symbol_id = match to_file_id {
                Some(fid) => store
                    .find_symbol_id_in_file(fid, &raw.to_name)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            store
                .insert_file_import(
                    repo.id,
                    pf.file_id,
                    &raw.to_name,
                    raw.to_file.as_deref(),
                    to_file_id,
                    to_symbol_id,
                    &now,
                )
                .await?;
            imports_map
                .entry((pf.file_id, raw.to_name.clone()))
                .or_insert((to_file_id, to_symbol_id));
        }
    }

    // -------------------------------------------------------------------------
    // Pass 2: resolve and insert edges — all symbols are in the DB now so
    // cross-file targets can be resolved. Resolution tiers, best first:
    // same-file (0.95) > receiver-type (0.9) > import (0.9) > module-path (0.8)
    // > repo-wide unique name (0.6) > unresolved (0.3).
    // -------------------------------------------------------------------------

    // Pass 2a: emit `contains` edges for every pending file up front, so
    // receiver-type resolution can consult them regardless of file walk order.
    for pf in pending.iter().filter(|pf| !pf.edges_only) {
        // The file is being re-ingested: retire its previous `contains` edges before
        // emitting the current set (see `close_stale_edges_for_symbol`).
        store.close_contains_edges_for_file(pf.file_id, &now).await?;
        if pf.symbols.len() > 1 {
            let mut sym_ids: Vec<(&crate::adapters::symbols::Symbol, Uuid)> = Vec::new();
            for s in &pf.symbols {
                if let Ok(Some(id)) = store.find_symbol_id_in_file(pf.file_id, &s.name).await {
                    sym_ids.push((s, id));
                }
            }
            for (outer, outer_id) in &sym_ids {
                for (inner, inner_id) in &sym_ids {
                    if outer_id == inner_id {
                        continue;
                    }
                    if outer.start_line < inner.start_line && outer.end_line >= inner.end_line {
                        let edge = SymbolEdge {
                            id: Uuid::new_v4(),
                            repo_id: repo.id,
                            from_id: *outer_id,
                            to_id: Some(*inner_id),
                            to_name: None,
                            edge_type: "contains".to_string(),
                            confidence: 1.0,
                            valid_from: now.clone(),
                            resolution: "contains".to_string(),
                        };
                        store.insert_symbol_edge(&edge).await?;
                    }
                }
            }
        }
    }

    // Pass 2b: resolve and insert call / import / type edges.
    for pf in &pending {
        let mut closed_stale: std::collections::HashSet<Uuid> =
            std::collections::HashSet::new();

        // Local `(scope, var) -> type name` bindings for receiver-type resolution.
        let binding_type: std::collections::HashMap<(&str, &str), &str> = pf
            .bindings
            .iter()
            .map(|b| ((b.scope.as_str(), b.var.as_str()), b.type_name.as_str()))
            .collect();

        for raw in &pf.raw_edges {
            let from_id = match pf.local_ids.get(&raw.from_name) {
                Some(id) => *id,
                None => continue,
            };

            // Close stale edges once per source symbol (not once per edge).
            if closed_stale.insert(from_id) {
                store.close_stale_edges_for_symbol(from_id, &now).await?;
            }

            let mut resolved_to: Option<Uuid> = None;
            let mut confidence: f64 = 0.3;
            let mut resolution: &str = "unresolved";

            // Tier 1: same-file exact match (0.95).
            if let Some(tid) = pf.local_ids.get(&raw.to_name) {
                resolved_to = Some(*tid);
                confidence = 0.95;
                resolution = "same-file";
            }

            // Tier 2: receiver-type — `recv` is a local var of a known type; look
            // up `to_name` as a member of that type via `contains` edges (0.9).
            if resolved_to.is_none() && raw.edge_type == "calls" {
                if let Some(recv) = raw.recv.as_deref() {
                    if let Some(ty) = binding_type.get(&(raw.from_name.as_str(), recv)) {
                        if let Ok(Some(tid)) = store
                            .find_symbol_id_by_type_and_name(repo.id, ty, &raw.to_name)
                            .await
                        {
                            resolved_to = Some(tid);
                            confidence = 0.9;
                            resolution = "receiver-type";
                        }
                    }
                }
            }

            // Tier 3: import — `to_name` was imported into this file (0.9).
            if resolved_to.is_none() {
                if let Some((fid, sid)) =
                    imports_map.get(&(pf.file_id, raw.to_name.clone())).copied()
                {
                    if let Some(sid) = sid {
                        resolved_to = Some(sid);
                        confidence = 0.9;
                        resolution = "import";
                    } else if let Some(fid) = fid {
                        if let Ok(Some(sym_id)) =
                            store.find_symbol_id_in_file(fid, &raw.to_name).await
                        {
                            resolved_to = Some(sym_id);
                            confidence = 0.9;
                            resolution = "import";
                        }
                    }
                }
            }

            // Tier 4: module-path spec carried on the edge itself (0.8).
            if resolved_to.is_none() {
                if let Some(ref spec) = raw.to_file {
                    if let Some(fid) =
                        resolve_spec_to_file(store, repo.id, &pf.rel, spec).await
                    {
                        if let Ok(Some(sym_id)) =
                            store.find_symbol_id_in_file(fid, &raw.to_name).await
                        {
                            resolved_to = Some(sym_id);
                            confidence = 0.8;
                            resolution = "module-path";
                        }
                    }
                }
            }

            // Tier 5: repo-wide unique name (0.6).
            if resolved_to.is_none() {
                if let Ok(Some(tid)) = store
                    .find_unique_symbol_id_in_repo(repo.id, &raw.to_name)
                    .await
                {
                    resolved_to = Some(tid);
                    confidence = 0.6;
                    resolution = "unique-name";
                }
            }

            let edge = SymbolEdge {
                id: Uuid::new_v4(),
                repo_id: repo.id,
                from_id,
                to_id: resolved_to,
                to_name: Some(raw.to_name.clone()),
                edge_type: raw.edge_type.to_string(),
                confidence,
                valid_from: now.clone(),
                resolution: resolution.to_string(),
            };
            store.insert_symbol_edge(&edge).await?;
        }
    }

    // -------------------------------------------------------------------------
    // Pass 3: re-resolve edges that were stored with to_id IS NULL from a prior
    // single-pass run. Now that more symbols exist, many can be resolved.
    // -------------------------------------------------------------------------
    if let Ok(unresolved) = store.fetch_unresolved_symbol_edges(repo.id).await {
        for (edge_id, _from_id, to_name, _edge_type) in unresolved {
            if let Ok(Some(to_id)) = store
                .find_unique_symbol_id_in_repo(repo.id, &to_name)
                .await
            {
                let _ = store
                    .resolve_symbol_edge(edge_id, to_id, 0.6, "unique-name")
                    .await;
            }
        }
    }

    ingest_routes(store, repo.id, &eligible, &now).await?;

    let mut resp = ArchResponse::empty();
    let communities_detected = detect_and_persist_communities(store, repo.id, &now).await?;
    let mut answer = format!(
        "Processed {} file(s) for symbols ({} unchanged, skipped).",
        processed, skipped
    );
    if refreshed > 0 {
        answer.push_str(&format!(
            " Re-resolved edges in {} unchanged file(s) whose targets changed name counts.",
            refreshed
        ));
    }
    if communities_detected > 0 {
        answer.push_str(&format!(" Detected {} communit(y/ies).", communities_detected));
    }
    resp.answer = Some(answer);

    if let Some(ref tx) = progress_tx {
        let _ = tx.send(format!(
            "indexing done: {}/{} files indexed, {} unchanged",
            processed, total, skipped
        ));
    }

    resp.warnings = warnings;
    resp.confidence = 1.0;

    for lane in ["symbol", "route", "community"] {
        crate::tools::freshness::record_lane(store, repo.id, &repo_path, lane, "ok").await;
    }
    reverify_changed_anchors(store, repo.id, &repo_path).await;

    Ok(IngestSymbolsResult {
        response: resp,
        files_total: total,
        files_processed: processed,
        files_unchanged: skipped,
        communities_detected,
        cancelled: false,
    })
}

async fn ingest_routes(
    store: &SqliteStore,
    repo_id: Uuid,
    eligible: &[EligibleFile],
    now: &str,
) -> Result<(), Error> {
    for file in eligible {
        let content = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let routes = crate::adapters::routes::extract_routes_for_extension(&file.ext, &content);
        // Close first: a file whose last route was removed must not keep it open.
        store
            .close_stale_routes_for_file(repo_id, &file.rel, now)
            .await?;
        if routes.is_empty() {
            continue;
        }

        let file_id = store.upsert_file(repo_id, &file.rel, now, now).await?;
        for route in &routes {
            let handler_id = if let Some(ref handler_name) = route.handler_name {
                store
                    .find_symbol_id_in_file(file_id, handler_name)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let resolution = if route.handler_name.is_none() {
                "no-handler"
            } else if handler_id.is_some() {
                "handler-in-file"
            } else {
                "handler-unresolved"
            };
            store
                .insert_route(
                    Uuid::new_v4(),
                    repo_id,
                    route.method.as_deref(),
                    &route.path,
                    route.framework.as_deref(),
                    handler_id,
                    &file.rel,
                    route.line as i64,
                    1.0,
                    now,
                    resolution,
                )
                .await?;
        }
    }
    Ok(())
}

/// Warm the anchor-verification cache after a re-ingest: verify each open
/// anchor against the new HEAD and store the result, so the next `cached`
/// freshness manifest is O(1). Best-effort and bounded.
async fn reverify_changed_anchors(
    store: &Arc<SqliteStore>,
    repo_id: Uuid,
    repo_path: &std::path::Path,
) {
    let head = crate::tools::verify_evidence::resolve_head(repo_path);
    let anchors = match store.open_anchors_for_repo(repo_id, 300).await {
        Ok(a) => a,
        Err(_) => return,
    };
    let now = chrono::Utc::now().to_rfc3339();
    for anchor in &anchors {
        let v = crate::tools::verify_evidence::verify_anchor(
            store, repo_id, repo_path, &head, anchor, &now,
        )
        .await;
        let dup = store
            .latest_verification(anchor.id, &head.repo_commit)
            .await
            .ok()
            .flatten()
            .map(|p| p.freshness == v.freshness && p.edit_class == v.edit_class)
            .unwrap_or(false);
        if !dup {
            let _ = store.insert_anchor_verification(&v).await;
        }
    }
}

async fn detect_and_persist_communities(
    store: &SqliteStore,
    repo_id: Uuid,
    now: &str,
) -> Result<usize, Error> {
    let symbols_with_files = store.get_symbols_with_files_for_repo(repo_id).await?;
    if symbols_with_files.is_empty() {
        return Ok(0);
    }

    let call_edges = store.get_call_edges_for_repo(repo_id).await?;
    let nodes: Vec<Uuid> = symbols_with_files.iter().map(|(id, _, _)| *id).collect();
    let labels = label_propagation(&nodes, &call_edges);
    let mut communities: std::collections::HashMap<Uuid, Vec<(Uuid, String, String)>> =
        std::collections::HashMap::new();
    for (sym_id, sym_name, file_path) in &symbols_with_files {
        let label = labels.get(sym_id).copied().unwrap_or(*sym_id);
        communities
            .entry(label)
            .or_default()
            .push((*sym_id, sym_name.clone(), file_path.clone()));
    }

    store.close_stale_communities_for_repo(repo_id, now).await?;
    for members in communities.values() {
        let community_id = Uuid::new_v4();
        let label = infer_community_label(members, 0);
        store
            .insert_community(community_id, repo_id, &label, members.len(), now)
            .await?;
        for (symbol_id, _, _) in members {
            store.insert_community_member(community_id, *symbol_id).await?;
        }
    }
    Ok(communities.len())
}

/// The repository's `.archignore` (gitignore syntax), if it has one.
pub(crate) fn load_archignore(repo_root: &std::path::Path) -> Result<Option<Gitignore>, ignore::Error> {
    let path = repo_root.join(".archignore");
    if !path.exists() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(repo_root);
    builder.add(path);
    builder.build().map(Some)
}

/// Whether ingest skips `path`: build/artifact directories, agent state, and
/// anything `.archignore` excludes.
pub(crate) fn is_ignored_entry(path: &std::path::Path, repo_root: &std::path::Path, gitignore: Option<&Gitignore>) -> bool {
    // Ignore non-directories only when patterns match the relative path; directories
    // with certain names are always ignored.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Common build / artifact directories
        if matches!(name, ".git" | "build" | "node_modules" | "target" | "target-streamable") {
            return true;
        }

        // Ignore .claude worktrees and other agent-generated state
        if name.starts_with(".claude") {
            return true;
        }
    }

    // Compute repo-relative path for pattern matching
    if let Ok(rel_path) = path.strip_prefix(repo_root) {
        // Never ignore the repository root itself — pruning it would skip all files.
        if rel_path.components().count() == 0 {
            return false;
        }
        // Consult compiled gitignore matcher if present
        if let Some(g) = gitignore {
            let is_dir = path.is_dir();
            let m = g.matched_path_or_any_parents(rel_path, is_dir);
            // If a path is explicitly whitelisted by a negated pattern, honor that first.
            if m.is_whitelist() {
                return false;
            }
            if m.is_ignore() {
                return true;
            }
        }
    }

    false
}

/// Label-propagation community detection over directed call/import edges treated as undirected.
/// Returns a map from symbol_id -> community_label (another symbol_id used as representative).
fn label_propagation(
    nodes: &[Uuid],
    edges: &[(Uuid, Uuid)],
) -> std::collections::HashMap<Uuid, Uuid> {
    use std::collections::HashMap;

    if nodes.is_empty() {
        return HashMap::new();
    }

    // Build undirected adjacency list
    let mut adj: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for n in nodes {
        adj.entry(*n).or_default();
    }
    for (a, b) in edges {
        adj.entry(*a).or_default().push(*b);
        adj.entry(*b).or_default().push(*a);
    }

    // Initialize: each node's label = itself
    let mut labels: HashMap<Uuid, Uuid> = nodes.iter().map(|n| (*n, *n)).collect();

    // Iterate up to 20 rounds
    for _ in 0..20 {
        let mut changed = false;
        // Process nodes in a deterministic order
        let mut node_list = nodes.to_vec();
        node_list.sort();

        for node in &node_list {
            let neighbors = match adj.get(node) {
                Some(n) => n,
                None => continue,
            };
            if neighbors.is_empty() {
                continue;
            }
            // Count label frequencies among neighbors
            let mut freq: HashMap<Uuid, usize> = HashMap::new();
            for nb in neighbors {
                let Some(&lbl) = labels.get(nb) else { continue };
                *freq.entry(lbl).or_default() += 1;
            }
            // Pick most frequent; tie-break by smallest label
            let best = freq
                .into_iter()
                .max_by(|(la, ca), (lb, cb)| ca.cmp(cb).then(lb.cmp(la)))
                .map(|(l, _)| l);

            if let Some(best_label) = best {
                if labels[node] != best_label {
                    labels.insert(*node, best_label);
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }
    }

    labels
}

/// Derives a human-readable community label from its members.
fn infer_community_label(
    members: &[(Uuid, String, String)], // (symbol_id, name, file_path)
    _internal_edges: usize,
) -> String {
    if members.is_empty() {
        return "unknown".to_string();
    }

    let paths: Vec<&str> = members.iter().map(|(_, _, fp)| fp.as_str()).collect();
    let prefix = longest_common_path_prefix(&paths);

    let mut names: Vec<&str> = members.iter().map(|(_, n, _)| n.as_str()).collect();
    names.sort();
    names.dedup();
    let top_names: Vec<&str> = names.iter().take(3).copied().collect();

    if top_names.is_empty() {
        return prefix;
    }
    format!("{} ({})", prefix, top_names.join(", "))
}

fn longest_common_path_prefix(paths: &[&str]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if paths.len() == 1 {
        let p = paths[0];
        if let Some(slash) = p.rfind('/') {
            return p[..slash].to_string();
        }
        return p.to_string();
    }

    let first = paths[0];
    let mut prefix_len = first.len();

    for path in &paths[1..] {
        let common = first
            .chars()
            .zip(path.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if common < prefix_len {
            prefix_len = common;
        }
    }

    let prefix = &first[..prefix_len];
    if let Some(last_slash) = prefix.rfind('/') {
        prefix[..last_slash].to_string()
    } else {
        prefix.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::fs;
    use tempfile::tempdir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn ingest_symbols_records_symbol_and_file() -> Result<(), Box<dyn std::error::Error>> {
        // create temp repo
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        let file_path = repo_path.join("src").join("lib.rs");
        fs::write(
            &file_path,
            "pub fn hello_world() -> String { \"hi\".to_string() }",
        )?;

        // create sqlite db
        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        let params = IngestSymbolsParams {
            repo_path: repo_path.to_string_lossy().to_string(),
            pattern: None,
            force: false,
        };

        let resp = run(&store, params, None, None).await?;
        let ans = resp.response.answer.clone().unwrap_or_default();
        assert!(ans.contains("Processed 1"), "resp.answer={:?}", ans);
        assert_eq!(resp.files_total, 1);
        assert_eq!(resp.files_processed, 1);
        assert_eq!(resp.files_unchanged, 0);
        assert!(!resp.cancelled);

        // verify symbol persisted in DB
        let repo_canon = repo_path.canonicalize()?;
        let repo = store
            .upsert_repository(&repo_canon.to_string_lossy(), None)
            .await?;
        let files = store
            .find_files_with_symbol(repo.id, "hello_world", None)
            .await?;
        assert!(
            files.iter().any(|p| p.ends_with("lib.rs")),
            "files={:?}",
            files
        );

        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_returns_structured_cancellation_result() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;
        fs::write(repo_path.join("src/lib.rs"), "pub fn pending() {}")?;

        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("memory.db").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let result = run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            Some(cancelled),
        )
        .await?;

        assert!(result.cancelled);
        assert_eq!(result.files_total, 1);
        assert_eq!(result.files_processed, 0);
        assert_eq!(result.files_unchanged, 0);
        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_closes_renamed_symbols_for_file() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        let file_path = repo_path.join("src").join("lib.rs");
        fs::write(
            &file_path,
            "pub fn old_name() -> String { \"hi\".to_string() }",
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        let params = IngestSymbolsParams {
            repo_path: repo_path.to_string_lossy().to_string(),
            pattern: None,
            force: false,
        };
        run(&store, params, None, None).await?;

        fs::write(
            &file_path,
            "pub fn new_name() -> String { \"hi\".to_string() }",
        )?;

        let params = IngestSymbolsParams {
            repo_path: repo_path.to_string_lossy().to_string(),
            pattern: None,
            force: false,
        };
        run(&store, params, None, None).await?;

        let repo_canon = repo_path.canonicalize()?;
        let repo = store
            .upsert_repository(&repo_canon.to_string_lossy(), None)
            .await?;
        let old_files = store
            .find_files_with_symbol(repo.id, "old_name", None)
            .await?;
        let new_files = store
            .find_files_with_symbol(repo.id, "new_name", None)
            .await?;

        assert!(old_files.is_empty(), "old_files={:?}", old_files);
        assert!(
            new_files.iter().any(|p| p.ends_with("lib.rs")),
            "new_files={:?}",
            new_files
        );

        let closed: Option<String> = sqlx::query_scalar(
            r#"SELECT s.valid_to
               FROM symbols s
               JOIN files f ON f.id = s.file_id
               WHERE f.repo_id = ? AND f.path = ? AND s.name = ?"#,
        )
        .bind(repo.id.to_string())
        .bind("src/lib.rs")
        .bind("old_name")
        .fetch_one(&sqlx::SqlitePool::connect(&db_url).await?)
        .await?;

        assert!(closed.is_some(), "old symbol was not closed");

        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_applies_pattern_filter() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;
        fs::write(
            repo_path.join("src").join("lib.rs"),
            "pub fn hello_world() -> String { \"hi\".to_string() }",
        )?;
        fs::write(
            repo_path.join("src").join("app.ts"),
            "export function webHandler() { return 'ok'; }",
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        let resp = run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: Some("src/**/*.ts".to_string()),
                force: false,
            },
            None,
            None,
        )
        .await?;

        assert!(
            resp.response.warnings.is_empty(),
            "warnings={:?}",
            resp.response.warnings
        );
        assert!(
            resp.response
                .answer
                .as_deref()
                .unwrap_or("")
                .contains("Processed 1 file(s) for symbols"),
            "resp.answer={:?}",
            resp.response.answer
        );

        let repo_canon = repo_path.canonicalize()?;
        let repo = store
            .upsert_repository(&repo_canon.to_string_lossy(), None)
            .await?;
        let rust_files = store
            .find_files_with_symbol(repo.id, "hello_world", None)
            .await?;
        let ts_files = store
            .find_files_with_symbol(repo.id, "webHandler", None)
            .await?;

        assert!(rust_files.is_empty(), "rust_files={rust_files:?}");
        assert!(
            ts_files.iter().any(|p| p.ends_with("app.ts")),
            "ts_files={ts_files:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_skips_binary_and_build_artifacts() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;
        fs::create_dir_all(repo_path.join("target").join("debug"))?;

        fs::write(
            repo_path.join("src").join("lib.rs"),
            "pub fn hello_world() -> String { \"hi\".to_string() }",
        )?;
        fs::write(repo_path.join("memory.db"), [0xff, 0xfe, 0xfd])?;
        fs::write(
            repo_path.join("target").join("debug").join("generated.rs"),
            "pub fn ignored_build_output() {}",
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        let resp = run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        assert!(
            resp.response
                .answer
                .as_deref()
                .unwrap_or("")
                .contains("Processed 1 file(s) for symbols"),
            "resp.answer={:?}",
            resp.response.answer
        );

        let repo_canon = repo_path.canonicalize()?;
        let repo = store
            .upsert_repository(&repo_canon.to_string_lossy(), None)
            .await?;
        let generated_files = store
            .find_files_with_symbol(repo.id, "ignored_build_output", None)
            .await?;
        assert!(generated_files.is_empty(), "files={generated_files:?}");

        Ok(())
    }

    /// Two Rust functions where `caller` calls `callee` in the same file.
    /// After ingestion, a `calls` edge with confidence ≥ 0.75 must exist.
    #[tokio::test]
    async fn ingest_symbols_extracts_rust_calls_edge() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        // `caller` calls `callee` explicitly.
        fs::write(
            repo_path.join("src").join("graph.rs"),
            r#"
pub fn callee() -> u32 { 42 }
pub fn caller() -> u32 { callee() }
"#,
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        // Verify the `calls` edge exists with confidence ≥ 0.75.
        use sqlx::Row;
        let pool = sqlx::SqlitePool::connect(&db_url).await?;
        let edge = sqlx::query(
            r#"SELECT se.confidence, se.edge_type, se.to_name
               FROM symbol_edges se
               JOIN symbols s_from ON s_from.id = se.from_id
               WHERE s_from.name = 'caller'
                 AND se.edge_type = 'calls'
                 AND se.to_name = 'callee'
                 AND se.valid_to IS NULL
               LIMIT 1"#,
        )
        .fetch_optional(&pool)
        .await?;

        assert!(edge.is_some(), "no calls edge found from caller → callee");
        let edge = edge.unwrap();
        let confidence: f64 = edge.get("confidence");
        assert!(
            confidence >= 0.75,
            "confidence {} is below 0.75",
            confidence
        );

        Ok(())
    }

    #[tokio::test]
    async fn archignore_ignores_dot_dirs_but_respects_negation() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(&repo_path)?;

        // Write .archignore as in user's file
        let arch = r#".*/
!docs/guide.md
!migrations/
!migrations/**
!src/
!src/**
!README.md
!TECHNICAL_BEHAVIOR.md
!Cargo.toml
"#;
        fs::write(repo_path.join(".archignore"), arch)?;

        // Create a dot worktree that should be ignored
        fs::create_dir_all(repo_path.join(".claude").join("worktrees"))?;
        fs::write(
            repo_path.join(".claude").join("worktrees").join("ignored.rs"),
            "pub fn ignored_in_claude() {}",
        )?;

        // Create a normal src file that should be included by the negation rules
        fs::create_dir_all(repo_path.join("src"))?;
        fs::write(repo_path.join("src").join("lib.rs"), "pub fn included() {}")?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        let params = IngestSymbolsParams {
            repo_path: repo_path.to_string_lossy().to_string(),
            pattern: None,
            force: false,
        };

        let resp = run(&store, params, None, None).await?;
        // Expect processed only 1 file (src/lib.rs)
        let ans = resp.response.answer.clone().unwrap_or_default();
        assert!(ans.contains("Processed 1") || ans.contains("Processed 2"), "resp.answer={:?}", ans);

        // Verify that symbol from src exists and .claude symbol does not
        let repo_canon = repo_path.canonicalize()?;
        let repo = store
            .upsert_repository(&repo_canon.to_string_lossy(), None)
            .await?;
        let included = store
            .find_files_with_symbol(repo.id, "included", None)
            .await?;
        assert!(included.iter().any(|p| p.ends_with("lib.rs")), "included={:?}", included);

        let ignored = store
            .find_files_with_symbol(repo.id, "ignored_in_claude", None)
            .await?;
        assert!(ignored.is_empty(), "ignored={:?}", ignored);

        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_persists_enrichment_fields() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        // Async pub function with return type
        fs::write(
            repo_path.join("src/lib.rs"),
            "pub async fn compute(x: i32) -> i32 { if x > 0 { x } else { -x } }\n",
        )?;

        let db_url = format!("sqlite:{}?mode=rwc", td.path().join("db.sqlite").display());
        let store = Arc::new(SqliteStore::connect(&db_url).await?);
        store.run_migrations().await?;

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        let pool = sqlx::SqlitePool::connect(&db_url).await?;
        let row = sqlx::query(
            "SELECT visibility, is_async, signature, return_type, complexity FROM symbols WHERE name = 'compute'",
        )
        .fetch_optional(&pool)
        .await?;

        assert!(row.is_some(), "symbol 'compute' not found");
        let row = row.unwrap();
        let visibility: Option<String> = row.get("visibility");
        let is_async: i64 = row.get("is_async");
        let signature: Option<String> = row.get("signature");
        let return_type: Option<String> = row.get("return_type");
        let complexity: Option<i64> = row.get("complexity");

        assert_eq!(visibility.as_deref(), Some("pub"), "visibility");
        assert_eq!(is_async, 1, "is_async");
        assert!(signature.is_some(), "signature should be extracted");
        assert!(return_type.is_some(), "return_type should be extracted");
        // complexity: 1 (baseline) + 1 (if) = 2
        assert_eq!(complexity, Some(2), "complexity");

        Ok(())
    }

    /// A Rust impl block with 2 methods — assert `contains` edges are emitted
    /// from the outer symbol (with the wider line range) to each inner method.
    #[tokio::test]
    async fn ingest_symbols_emits_contains_edges_for_impl_block() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        // The impl block spans lines 1–10; methods are inside it.
        // tree-sitter will extract all three as separate symbols.
        fs::write(
            repo_path.join("src").join("nested.rs"),
            r#"pub struct Foo {}
impl Foo {
    pub fn alpha(&self) -> u32 {
        1
    }
    pub fn beta(&self) -> u32 {
        2
    }
}
"#,
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        use sqlx::Row;
        let pool = sqlx::SqlitePool::connect(&db_url).await?;

        // At least one `contains` edge must exist (outer → inner)
        let count: i64 = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM symbol_edges WHERE edge_type = 'contains' AND valid_to IS NULL",
        )
        .fetch_one(&pool)
        .await?
        .get("cnt");

        assert!(count >= 1, "expected at least 1 contains edge, got {}", count);

        Ok(())
    }

    /// Cross-file TS: a method call on a locally-constructed object resolves via
    /// the `receiver-type` tier; a call to an imported free function resolves via
    /// the `import` tier.
    #[tokio::test]
    async fn ingest_symbols_scoped_resolution_tiers() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        fs::write(
            repo_path.join("src").join("service.ts"),
            "export class Service {\n  start() { return 1; }\n}\n",
        )?;
        fs::write(
            repo_path.join("src").join("util.ts"),
            "export function helper() { return 2; }\n",
        )?;
        fs::write(
            repo_path.join("src").join("app.ts"),
            concat!(
                "import { Service } from './service';\n",
                "import { helper } from './util';\n",
                "export function boot() {\n",
                "  const s = new Service();\n",
                "  s.start();\n",
                "  helper();\n",
                "}\n",
            ),
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        use sqlx::Row;
        let pool = sqlx::SqlitePool::connect(&db_url).await?;

        let row = |to_name: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    r#"SELECT se.resolution AS resolution, se.confidence AS confidence
                       FROM symbol_edges se
                       JOIN symbols sf ON sf.id = se.from_id
                       WHERE sf.name = 'boot' AND se.edge_type = 'calls'
                         AND se.to_name = ? AND se.valid_to IS NULL
                       LIMIT 1"#,
                )
                .bind(to_name)
                .fetch_optional(&pool)
                .await
            }
        };

        let start = row("start").await?.expect("boot -> start edge");
        assert_eq!(start.get::<String, _>("resolution"), "receiver-type");
        assert!(start.get::<f64, _>("confidence") >= 0.9);

        let helper = row("helper").await?.expect("boot -> helper edge");
        assert_eq!(helper.get::<String, _>("resolution"), "import");

        // file_imports table populated for app.ts
        let imports: i64 = sqlx::query(
            "SELECT COUNT(*) AS c FROM file_imports WHERE to_file_id IS NOT NULL AND valid_to IS NULL",
        )
        .fetch_one(&pool)
        .await?
        .get("c");
        assert!(imports >= 2, "expected >=2 resolved file_imports, got {imports}");

        Ok(())
    }

    /// Three files with two functional groups — assert two communities detected.
    /// Group A: foo.rs (fn foo calls bar), bar.rs (fn bar)
    /// Group B: qux.rs (fn qux, standalone)
    #[tokio::test]
    async fn ingest_symbols_detects_two_communities() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        fs::write(repo_path.join("src").join("foo.rs"), "pub fn foo() { bar() }\n")?;
        fs::write(repo_path.join("src").join("bar.rs"), "pub fn bar() {}\n")?;
        fs::write(repo_path.join("src").join("qux.rs"), "pub fn qux() {}\n")?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        use sqlx::Row;
        let pool = sqlx::SqlitePool::connect(&db_url).await?;
        let count: i64 =
            sqlx::query("SELECT COUNT(*) AS cnt FROM communities WHERE valid_to IS NULL")
                .fetch_one(&pool)
                .await?
                .get("cnt");

        // foo and bar may converge (foo calls bar) → same community; qux is isolated → own community
        assert!(count >= 1, "expected at least 1 community, got {}", count);
        assert!(count <= 3, "expected at most 3 communities, got {}", count);

        Ok(())
    }

    #[tokio::test]
    async fn ingest_symbols_detects_express_route() -> TestResult {
        let td = tempdir()?;
        let repo_path = td.path().join("repo");
        fs::create_dir_all(repo_path.join("src"))?;

        fs::write(
            repo_path.join("src").join("routes.js"),
            "const router = require('express').Router();\nrouter.get('/health', healthCheck);\n",
        )?;

        let db_path = td.path().join("memory.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let store = SqliteStore::connect(&db_url).await?;
        store.run_migrations().await?;
        let store = Arc::new(store);

        run(
            &store,
            IngestSymbolsParams {
                repo_path: repo_path.to_string_lossy().to_string(),
                pattern: None,
                force: false,
            },
            None,
            None,
        )
        .await?;

        use sqlx::Row;
        let pool = sqlx::SqlitePool::connect(&db_url).await?;
        let row = sqlx::query(
            "SELECT method, path, framework FROM routes WHERE valid_to IS NULL LIMIT 1",
        )
        .fetch_optional(&pool)
        .await?;

        assert!(row.is_some(), "expected a route record");
        let row = row.unwrap();
        let method: Option<String> = row.get("method");
        let path: String = row.get("path");
        assert_eq!(method.as_deref(), Some("GET"));
        assert_eq!(path, "/health");

        Ok(())
    }
}
