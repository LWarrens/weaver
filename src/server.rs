use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, ErrorData, Implementation, LoggingLevel,
        LoggingMessageNotificationParam, ServerCapabilities, ServerInfo,
    },
    service::{NotificationContext, RoleServer},
    tool, tool_handler, tool_router, ServerHandler,
};

use crate::storage::SqliteStore;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
struct IngestStatusParams {
    /// The job_id returned by ingest_symbols.
    pub job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SynthLeadStatusParams {
    /// The job_id returned by synthesize_adr_leads.
    pub job_id: String,
}

use crate::tools::{
    adr_lineage::AdrLineageParams,
    check_consistency::CheckConsistencyParams,
    find_stale_decisions::FindStaleDecisionsParams,
    propose_links::ProposeLinksParams,
    retract::RetractParams,
    trace_symbol_history::TraceSymbolHistoryParams,
    architecture_query::ArchQueryParams,
    diff_architecture::DiffArchitectureParams,
    embed_all::EmbedAllParams,
    explain_answer::ExplainAnswerParams,
    sync_incremental::SyncIncrementalParams,
    find_decisions::FindDecisionsForCodeParams,
    generate_adr_draft::GenerateAdrDraftParams, generate_adr_patch::GenerateAdrPatchParams,
    delete_repo::DeleteRepoParams,
    get_architecture::GetArchitectureParams, get_graph_schema::GetGraphSchemaParams, list_repos::ListReposResult,
    get_graph_snapshot::GetGraphSnapshotParams,
    find_orphaned_code::FindOrphanedCodeParams,
    synthesize_adr_leads::SynthesizeAdrLeadsParams,
    impact_of::ImpactOfParams,
    index_status::IndexStatusParams,
    ingest_symbols::IngestSymbolsParams, inspect_change::InspectChangeParams,
    record_episode::RecordDecisionEpisodeParams, sync_adrs::SyncAdrsFromGitParams,
    sync_commits::SyncCommitsFromGitParams,
    trace_call_path::TraceCallPathParams,
    validate_adr::ValidateAdrParams,
    focused_file_brief::FileBriefParams,
    verify_claims::VerifyClaimsParams,
};

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Shared handle that lets any MCP tool (or an OS signal) trigger a daemon reload.
#[derive(Clone)]
pub struct ReloadHandle {
    stop: CancellationToken,
    reload_flag: Arc<AtomicBool>,
}

impl ReloadHandle {
    pub fn new(stop: CancellationToken, reload_flag: Arc<AtomicBool>) -> Self {
        Self { stop, reload_flag }
    }

    pub fn trigger(&self) {
        self.reload_flag.store(true, Ordering::Relaxed);
        self.stop.cancel();
    }
}

/// Lightweight handle for a background ingest job.
#[derive(Clone)]
struct IngestJob {
    total: Arc<std::sync::atomic::AtomicUsize>,
    processed: Arc<std::sync::atomic::AtomicUsize>,
    skipped: Arc<std::sync::atomic::AtomicUsize>,
    communities: Arc<std::sync::atomic::AtomicUsize>,
    done: Arc<std::sync::atomic::AtomicBool>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// Lightweight handle for a background synthesize_adr_leads job.
#[derive(Clone)]
struct SynthLeadJob {
    done: Arc<std::sync::atomic::AtomicBool>,
    result_json: Arc<std::sync::Mutex<Option<String>>>,
    error: Arc<std::sync::Mutex<Option<String>>>,
}

#[derive(Clone)]
pub struct ArchServer {
    store: Arc<SqliteStore>,
    // Read by rmcp's generated tool handler.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Set once when the MCP client sends `notifications/initialized`.
    peer: Arc<std::sync::OnceLock<rmcp::service::Peer<RoleServer>>>,
    /// Active background ingest jobs, keyed by job ID.
    jobs: Arc<std::sync::Mutex<std::collections::HashMap<String, IngestJob>>>,
    /// Active background synthesize_adr_leads jobs, keyed by job ID.
    synth_jobs: Arc<std::sync::Mutex<std::collections::HashMap<String, SynthLeadJob>>>,
    /// Present only in daemon mode; used by the `reload_daemon` tool.
    reload: Option<ReloadHandle>,
}

impl ArchServer {
    pub fn new(store: SqliteStore) -> Self {
        Self::with_reload(store, None)
    }

    pub fn with_reload(store: SqliteStore, reload: Option<ReloadHandle>) -> Self {
        ArchServer {
            store: Arc::new(store),
            tool_router: Self::tool_router(),
            peer: Arc::new(std::sync::OnceLock::new()),
            jobs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            synth_jobs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            reload,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations (proc-macro dispatch)
// ---------------------------------------------------------------------------

#[tool_router]
impl ArchServer {
    /// Scan ADR markdown files from a repository path, parse structured fields
    /// (title, status, date, context, decision, consequences, supersedes), and
    /// ingest them into the architectural memory store. Creates decision records,
    /// constraint records, and decision ↔ code file links from explicit mentions.
    #[tool(
        name = "sync_adrs_from_git",
        description = "Scan ADR markdown files from a repository, parse structured fields, and ingest them into architectural memory."
    )]
    async fn sync_adrs_from_git(
        &self,
        Parameters(params): Parameters<SyncAdrsFromGitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::sync_adrs::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Walk git commits on a branch and ingest them into architectural memory.
    /// Links commits to decisions by ADR ID reference or keyword overlap.
    #[tool(
        name = "sync_commits_from_git",
        description = "Ingest git commits from a repository into architectural memory and link them to related decisions."
    )]
    async fn sync_commits_from_git(
        &self,
        Parameters(params): Parameters<SyncCommitsFromGitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::sync_commits::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Resolve a file path to the decisions and constraints that govern it.
    /// Returns only decisions that explicitly mention the file in their ADR text
    /// (Phase 1). Symbol, module, and service resolution are available in Phase 2.
    #[tool(
        name = "find_decisions_for_code",
        description = "Find active architectural decisions and constraints that apply to a given file, symbol, module, or service."
    )]
    async fn find_decisions_for_code(
        &self,
        Parameters(params): Parameters<FindDecisionsForCodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::find_decisions::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Ingest source-file symbols (Phase 2). Uses tree-sitter to extract symbols and stores
    /// them. Runs in the background — returns a job_id immediately. Progress notifications
    /// are sent as MCP log messages. Use `ingest_symbols_status` to check completion.
    /// Pass `force: true` to re-index all files regardless of whether their content changed.
    #[tool(
        name = "ingest_symbols",
        description = "Ingest source-file symbols using tree-sitter. Runs in the background and streams progress. Returns a job_id immediately."
    )]
    async fn ingest_symbols(
        &self,
        Parameters(params): Parameters<IngestSymbolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let job_id = uuid::Uuid::new_v4().to_string();
        let job = IngestJob {
            total: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            processed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            skipped: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            communities: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        self.jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), job.clone());

        // Channel: ingest task → logger task
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let peer_opt = self.peer.get().cloned();

        // Logger task: reads progress messages and forwards them to the MCP client as
        // log-level notifications. Runs until the sender is dropped (ingest completes).
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Some(ref peer) = peer_opt {
                    let _ = peer
                        .notify_logging_message(LoggingMessageNotificationParam::new(
                            LoggingLevel::Info,
                            serde_json::Value::String(msg),
                        ))
                        .await;
                }
            }
        });

        let store = self.store.clone();
        let job_done = job.done.clone();
        let job_failed = job.failed.clone();
        let job_total = job.total.clone();
        let job_processed = job.processed.clone();
        let job_skipped = job.skipped.clone();
        let job_communities = job.communities.clone();
        let job_cancel = job.cancelled.clone();
        let jid = job_id.clone();

        // Ingest task: does the actual work, reports progress via channel.
        tokio::spawn(async move {
            match crate::tools::ingest_symbols::run(&store, params, Some(tx), Some(job_cancel)).await {
                Ok(resp) => {
                    tracing::info!(job_id = %jid, answer = ?resp.response.answer, "ingest_symbols completed");
                    job_total.store(resp.files_total, std::sync::atomic::Ordering::Relaxed);
                    job_processed.store(resp.files_processed, std::sync::atomic::Ordering::Relaxed);
                    job_skipped.store(resp.files_unchanged, std::sync::atomic::Ordering::Relaxed);
                    job_communities.store(resp.communities_detected, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::error!(job_id = %jid, error = %e, "ingest_symbols failed");
                    job_failed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            job_done.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Indexing started (job_id: {}). Progress will appear as log messages. \
             Call ingest_symbols_status with this job_id to check completion.",
            job_id
        ))]))
    }

    /// Check the status of a background ingest_symbols job.
    #[tool(
        name = "ingest_symbols_status",
        description = "Check whether a background ingest_symbols job has completed."
    )]
    async fn ingest_symbols_status(
        &self,
        Parameters(params): Parameters<IngestStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(&params.job_id) {
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "No job found with id '{}'.",
                params.job_id
            ))])),
            Some(job) => {
                let done = job.done.load(std::sync::atomic::Ordering::Relaxed);
                let failed = job.failed.load(std::sync::atomic::Ordering::Relaxed);
                let cancelled = job.cancelled.load(std::sync::atomic::Ordering::Relaxed);
                let total = job.total.load(std::sync::atomic::Ordering::Relaxed);
                let processed = job.processed.load(std::sync::atomic::Ordering::Relaxed);
                let skipped = job.skipped.load(std::sync::atomic::Ordering::Relaxed);
                let communities = job.communities.load(std::sync::atomic::Ordering::Relaxed);
                let status = if cancelled {
                    "cancelled"
                } else if failed {
                    "failed"
                } else if done {
                    "done"
                } else {
                    "running"
                };
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "job_id={} status={} total={} processed={} skipped={} communities={}",
                    params.job_id, status, total, processed, skipped, communities
                ))]))
            }
        }
    }

    /// Cancel a running background ingest_symbols job.
    #[tool(
        name = "cancel_ingest",
        description = "Cancel a running background ingest_symbols job. The job stops cleanly after the current file finishes."
    )]
    async fn cancel_ingest(
        &self,
        Parameters(params): Parameters<IngestStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(&params.job_id) {
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "No job found with id '{}'.",
                params.job_id
            ))])),
            Some(job) => {
                let already_done = job.done.load(std::sync::atomic::Ordering::Relaxed);
                if already_done {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "job_id={} status=done — job already completed, nothing to cancel.",
                        params.job_id
                    ))]));
                }
                job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "job_id={} status=cancelling — will stop after the current file.",
                    params.job_id
                ))]))
            }
        }
    }

    /// Inspect a set of changed files and symbols against active architectural decisions.
    /// Returns possible constraint violations with confidence scores.
    #[tool(
        name = "inspect_change_against_decisions",
        description = "Inspect changed files or symbols against architectural decisions and surface constraint violations."
    )]
    async fn inspect_change_against_decisions(
        &self,
        Parameters(params): Parameters<InspectChangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::inspect_change::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Record an architectural decision episode from a discussion, PR comment,
    /// or meeting note. Uses the configured LLM provider to extract structured
    /// facts (decisions + constraints) from free-form text.
    #[tool(
        name = "record_decision_episode",
        description = "Record an architectural event or decision discussion as an episode. Extracts structured decisions and constraints from free-form text via LLM."
    )]
    async fn record_decision_episode(
        &self,
        Parameters(params): Parameters<RecordDecisionEpisodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::record_episode::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Query architectural decisions and constraints by keyword or topic.
    /// Returns matching decisions with their constraints.
    #[tool(
        name = "query",
        description = "Search architectural decisions and constraints by keyword or topic."
    )]
    async fn architecture_query(
        &self,
        Parameters(params): Parameters<ArchQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::architecture_query::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Generate a Markdown ADR draft with sequential ID, template sections, and
    /// links to related existing decisions.
    #[tool(
        name = "generate_adr_draft",
        description = "Generate a Markdown ADR draft with the next sequential ID, pre-filled sections, and links to related decisions."
    )]
    async fn generate_adr_draft(
        &self,
        Parameters(params): Parameters<GenerateAdrDraftParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::generate_adr_draft::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Produce a git-ready patch for a new or updated ADR.
    #[tool(
        name = "generate_adr_patch",
        description = "Produce a git-ready unified diff patch for a new or updated ADR without writing files."
    )]
    async fn generate_adr_patch(
        &self,
        Parameters(params): Parameters<GenerateAdrPatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::generate_adr_patch::run(params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Return the concrete SQLite graph schema exposed by the current server.
    #[tool(
        name = "get_graph_schema",
        description = "Return the concrete SQLite-backed graph schema, table roles, columns, and optional row counts."
    )]
    async fn get_graph_schema(
        &self,
        Parameters(params): Parameters<GetGraphSchemaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::get_graph_schema::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Return all repositories currently stored in the architectural memory database.
    #[tool(
        name = "list_repos",
        description = "List all repositories stored in the architectural memory database."
    )]
    async fn list_repos(
        &self,
    ) -> Result<CallToolResult, ErrorData> {
        let result: ListReposResult = crate::tools::list_repos::run(&self.store)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Permanently delete a repository and all its associated data from the database.
    #[tool(
        name = "delete_repo",
        description = "Permanently delete a repository and all associated architectural data (symbols, decisions, commits, ADRs) from the database. This action is irreversible."
    )]
    async fn delete_repo(
        &self,
        Parameters(params): Parameters<DeleteRepoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::delete_repo::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Return a node/edge snapshot for interactive graph inspection.
    #[tool(
        name = "get_graph_snapshot",
        description = "Return a node-edge graph snapshot for interactive relationship visualization."
    )]
    async fn get_graph_snapshot(
        &self,
        Parameters(params): Parameters<GetGraphSnapshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::get_graph_snapshot::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Return a high-level summary of currently ingested architectural memory for a repo.
    #[tool(
        name = "get_architecture",
        description = "Summarize the currently ingested architecture for a repository using decisions, constraints, symbols, and warnings."
    )]
    async fn get_architecture(
        &self,
        Parameters(params): Parameters<GetArchitectureParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::get_architecture::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Traverse the call graph (symbol_edges) to answer who calls a symbol
    /// (inbound) or what it calls (outbound), up to a configurable depth.
    #[tool(
        name = "trace_call_path",
        description = "Traverse the call graph to find callers (inbound) or callees (outbound) of a symbol up to a given depth."
    )]
    async fn trace_call_path(
        &self,
        Parameters(params): Parameters<TraceCallPathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::trace_call_path::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// BFS traversal of temporal_edges to determine what a decision impacts.
    #[tool(
        name = "impact_of",
        description = "Transitively traverse temporal edges from an ADR/decision to discover what it affects."
    )]
    async fn impact_of(
        &self,
        Parameters(params): Parameters<ImpactOfParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::impact_of::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        let content = Content::json(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    /// Backfill vector embeddings for all decisions, constraints, episodes, commits, and symbols
    /// that do not yet have an embedding. Requires an embedding provider to be configured via
    /// WEAVER_EMBEDDING_PROVIDER (or --embedding-provider).
    #[tool(
        name = "embed_all",
        description = "Backfill vector embeddings for all unembedded decisions, constraints, episodes, commits, and symbols in a repository."
    )]
    async fn embed_all(
        &self,
        Parameters(params): Parameters<EmbedAllParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::embed_all::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Show indexing freshness, entity counts, and embedding coverage for a repository.
    #[tool(
        name = "index_status",
        description = "Show indexing coverage, embedding completeness, and last sync timestamps for a repository."
    )]
    async fn index_status(
        &self,
        Parameters(params): Parameters<IndexStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::index_status::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Identify files and symbols that lack governing architectural decisions or constraints.
    #[tool(
        name = "find_orphaned_code",
        description = "Find files and symbols with no linked ADRs, decisions, or constraints. Surfaces architectural dark matter."
    )]
    async fn find_orphaned_code(
        &self,
        Parameters(params): Parameters<FindOrphanedCodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::find_orphaned_code::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Record observed architectural patterns for orphaned files/symbols using an LLM.
    /// Runs in the background — returns a job_id immediately. Poll with
    /// `synthesize_adr_leads_status` to retrieve results when done.
    #[tool(
        name = "synthesize_adr_leads",
        description = "Observe and record undocumented architectural patterns for files lacking explicit decisions. Runs in the background and returns a job_id immediately. Poll synthesize_adr_leads_status for results."
    )]
    async fn synthesize_adr_leads(
        &self,
        Parameters(params): Parameters<SynthesizeAdrLeadsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let job_id = uuid::Uuid::new_v4().to_string();
        let job = SynthLeadJob {
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            result_json: Arc::new(std::sync::Mutex::new(None)),
            error: Arc::new(std::sync::Mutex::new(None)),
        };
        self.synth_jobs.lock().unwrap().insert(job_id.clone(), job.clone());

        let store = self.store.clone();
        let done = job.done.clone();
        let result_slot = job.result_json.clone();
        let error_slot = job.error.clone();
        let jid = job_id.clone();

        tokio::spawn(async move {
            match crate::tools::synthesize_adr_leads::run(&store, params).await {
                Ok(result) => {
                    let json = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| "{}".to_string());
                    *result_slot.lock().unwrap() = Some(json);
                    tracing::info!(job_id = %jid, "synthesize_adr_leads completed");
                }
                Err(e) => {
                    tracing::error!(job_id = %jid, error = %e, "synthesize_adr_leads failed");
                    *error_slot.lock().unwrap() = Some(e.to_string());
                }
            }
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Synthesis started (job_id: {}). Call synthesize_adr_leads_status with this job_id to poll for results.",
            job_id
        ))]))
    }

    /// Poll the result of a background synthesize_adr_leads job.
    #[tool(
        name = "synthesize_adr_leads_status",
        description = "Poll the result of a background synthesize_adr_leads job. Returns leads JSON when done, or status=running while in progress."
    )]
    async fn synthesize_adr_leads_status(
        &self,
        Parameters(params): Parameters<SynthLeadStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let jobs = self.synth_jobs.lock().unwrap();
        match jobs.get(&params.job_id) {
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "No synthesis job found with id '{}'.",
                params.job_id
            ))])),
            Some(job) => {
                let done = job.done.load(std::sync::atomic::Ordering::Relaxed);
                if !done {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "job_id={} status=running",
                        params.job_id
                    ))]));
                }
                if let Some(err) = job.error.lock().unwrap().as_deref() {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "job_id={} status=error error={}",
                        params.job_id, err
                    ))]));
                }
                if let Some(json) = job.result_json.lock().unwrap().as_deref() {
                    return Ok(CallToolResult::success(vec![Content::text(json.to_string())]));
                }
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "job_id={} status=unknown",
                    params.job_id
                ))]))
            }
        }
    }

    /// Validate an ADR file for structural completeness, duplicate IDs, and supersession consistency.
    #[tool(
        name = "validate_adr",
        description = "Validate an ADR markdown file for structural completeness, duplicate IDs, and supersession chain consistency."
    )]
    async fn validate_adr(
        &self,
        Parameters(params): Parameters<ValidateAdrParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::validate_adr::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Detect and explain conflicts across ADRs and decisions.
    #[tool(
        name = "check_consistency",
        description = "Cross-ADR consistency check: explicit conflicts_with edges, contradictory constraints (opposite obligation polarity over shared terms/files), and supersession inconsistencies. Returns explained conflict candidates with per-conflict confidence; read-only, never auto-resolves."
    )]
    async fn check_consistency(
        &self,
        Parameters(params): Parameters<CheckConsistencyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::check_consistency::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Query the ADR supersession chain for a given ADR ID.
    #[tool(
        name = "adr_lineage",
        description = "Query the ADR supersession chain for a given ADR ID. Returns what it superseded and what superseded it."
    )]
    async fn adr_lineage(
        &self,
        Parameters(params): Parameters<AdrLineageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::adr_lineage::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Show which decisions, constraints, and ADRs were added or removed between
    /// two points in time (ISO-8601 timestamps or git refs).
    #[tool(
        name = "diff_architecture",
        description = "Compare architectural state between two timestamps or git refs. Returns decisions, constraints, and ADRs added or removed in the window."
    )]
    async fn diff_architecture(
        &self,
        Parameters(params): Parameters<DiffArchitectureParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::diff_architecture::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Re-run a query with full retrieval provenance: which terms matched, which
    /// decisions were found via keyword vs. semantic vs. graph expansion, and how
    /// they were ranked by Reciprocal Rank Fusion.
    #[tool(
        name = "explain_answer",
        description = "Re-run a query with full provenance: keyword matches, semantic similarities, graph expansion steps, and RRF ranking scores."
    )]
    async fn explain_answer(
        &self,
        Parameters(params): Parameters<ExplainAnswerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::explain_answer::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Re-index only the files that changed since a given commit or timestamp,
    /// skipping unchanged ADRs and source files. Faster than a full sync for
    /// incremental CI runs or post-commit hooks.
    #[tool(
        name = "sync_incremental",
        description = "Re-index only files changed since a given git ref or timestamp. Faster than a full sync for CI hooks or post-commit runs."
    )]
    async fn sync_incremental(
        &self,
        Parameters(params): Parameters<SyncIncrementalParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::sync_incremental::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Soft-delete a hallucinated or incorrect LLM-extracted fact (decision,
    /// constraint, or all facts derived from an episode). Records an optional
    /// correction note as an audit episode. Idempotent: retracting an already-
    /// retracted entity adds a warning but does not error.
    #[tool(
        name = "retract",
        description = "Soft-delete a hallucinated or incorrect architectural fact (decision, constraint, or episode). Records an optional correction note."
    )]
    async fn retract(
        &self,
        Parameters(params): Parameters<RetractParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::retract::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Suggest candidate links between a target ADR or decision and related
    /// commits, symbols, decisions, and routes. All candidates are non-authoritative
    /// and include an explicit confidence score; none are promoted automatically.
    #[tool(
        name = "propose_links",
        description = "Suggest candidate links between an ADR or decision and related commits, symbols, decisions, and routes. All candidates are non-authoritative with explicit confidence scores."
    )]
    async fn propose_links(
        &self,
        Parameters(params): Parameters<ProposeLinksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::propose_links::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Detect accepted architectural decisions that may no longer align with
    /// implementation reality. Checks for deleted files, missing symbols, and
    /// decisions with no linked commit activity since a configurable cutoff.
    #[tool(
        name = "find_stale_decisions",
        description = "Detect accepted decisions that may no longer align with implementation. Checks deleted files, missing symbols, and no recent commit activity."
    )]
    async fn find_stale_decisions(
        &self,
        Parameters(params): Parameters<FindStaleDecisionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::find_stale_decisions::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Trace the temporal history of a symbol, file, or route across commits,
    /// decisions, constraints, and episodes. Returns a timeline of events sorted
    /// oldest to newest.
    #[tool(
        name = "trace_symbol_history",
        description = "Trace the temporal history of a symbol across commits, ADRs, decisions, constraints, and episodes."
    )]
    async fn trace_symbol_history(
        &self,
        Parameters(params): Parameters<TraceSymbolHistoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::trace_symbol_history::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Drain in-flight MCP sessions, re-create the server, and re-bind the port —
    /// all without dropping the listening socket, so clients see zero ECONNREFUSED.
    /// Only available when the binary is running in `--daemon` mode.
    #[tool(
        name = "reload_daemon",
        description = "Hot-reload the daemon: drain current sessions, rebuild the server, and continue serving — no port gap. Only works in --daemon mode."
    )]
    async fn reload_daemon(&self) -> Result<CallToolResult, ErrorData> {
        match &self.reload {
            None => Ok(CallToolResult::success(vec![Content::text(
                "reload_daemon is only available in --daemon mode",
            )])),
            Some(handle) => {
                handle.trigger();
                Ok(CallToolResult::success(vec![Content::text(
                    "reload triggered — draining current sessions and restarting",
                )]))
            }
        }
    }

    /// Return a compact LLM-optimised brief for a file or symbol: exported symbols
    /// with line anchors, cross-file callers and callees grouped by file, governing
    /// architectural decisions, and recent commits. Much cheaper than chaining
    /// get_architecture → trace_call_path → find_decisions_for_code separately.
    #[tool(
        name = "focused_file_brief",
        description = "Return a compact brief for a file or symbol: exports (name/kind/line), cross-file callers, callees, governing ADRs, and recent commits. Provide `file` (repo-relative path) or `symbol` (name)."
    )]
    async fn focused_file_brief(
        &self,
        Parameters(params): Parameters<FileBriefParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::focused_file_brief::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Full evidence detail for the claims of an ADR, decision, or file: every
    /// anchor, its verification against the current working tree, the claim's
    /// three-state disposition (unaffected / affected / unprovable), and whether
    /// its recorded read-set is fully anchored. Debugging counterpart to the
    /// inline `freshness` manifest on retrieval responses.
    #[tool(
        name = "verify_claims",
        description = "Verify the evidence anchors behind an ADR, decision, or file. Returns per-anchor freshness, per-claim disposition (unaffected/affected/unprovable), and completeness. Provide one of adr_id, decision_id, file, symbol."
    )]
    async fn verify_claims(
        &self,
        Parameters(params): Parameters<VerifyClaimsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::verify_claims::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Offline integrity oracle: rebuild the ADR index lane from scratch in a
    /// throwaway store and diff its declared-fact projection against the live
    /// index. Off the hot path — for CI or manual auditing, not queries.
    #[tool(
        name = "verify_index_integrity",
        description = "Audit index integrity: rebuild the ADR lane from scratch and diff it against the live index. Reports per-lane clean/divergent/not_audited plus the diverging claim projections. Run in CI, not on the query path."
    )]
    async fn verify_index_integrity(
        &self,
        Parameters(params): Parameters<
            crate::tools::verify_index_integrity::VerifyIndexIntegrityParams,
        >,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::tools::verify_index_integrity::run(&self.store, params)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ArchServer {
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        // Capture the peer so tool handlers can send log notifications.
        let _ = self.peer.set(context.peer.clone());
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "weaver",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                String::from("")
                    + "architectural informant. "
                    + "ORIENT: index_status(repo) → get_architecture(repo) → get_graph_schema; "
                    + "INGEST: sync_adrs_from_git → ingest_symbols (background, poll ingest_symbols_status) → sync_commits_from_git → embed_all; "
                    + "sync_incremental for incremental CI runs; "
                    + "QUERY: query(text) keyword+semantic search; find_decisions_for_code(file/symbol) governance lookup; explain_answer for retrieval provenance; "
                    + "ANALYSE: inspect_change_against_decisions(files) constraint check; impact_of(decision_id) BFS impact; trace_call_path(from,to) call graph; "
                    + "trace_symbol_history(symbol) temporal timeline; diff_architecture(repo,ref1,ref2) snapshot diff; "
                    + "find_stale_decisions(repo,since) drift detection; find_orphaned_code(repo) dark matter; propose_links(adr_id) candidate relationships; "
                    + "AUTHOR: generate_adr_draft → validate_adr → generate_adr_patch; adr_lineage(adr_id) supersession chain; "
                    + "synthesize_adr_leads after find_orphaned_code for undocumented patterns; "
                    + "CORRECT: retract(entity_type,entity_id) soft-delete incorrect facts; record_decision_episode for discussion/PR/meeting provenance; "
                    + "UTILITY: focused_file_brief(file|symbol) compact cross-reference brief; list_repos / delete_repo / reload_daemon; "
                    + "always attach explain_answer provenance; require explicit human approval before applying any patch or schema change. "
,
            )
    }
}
