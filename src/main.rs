use weaver::{daemon, server, storage, tools};

use clap::Parser;
use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const DEFAULT_CONFIG_FILE: &str = ".weaver.yaml";
const DEFAULT_BIND: &str = "127.0.0.1:3000";
const CONFIG_ENV_VARS: &[&str] = &[
    "WEAVER_DB",
    "WEAVER_BIND",
    "WEAVER_INDEX_REPO",
    "WEAVER_INDEX_PATTERN",
    "WEAVER_LLM_PROVIDER",
    "WEAVER_LLM_MODEL",
    "WEAVER_LLM_URL",
    "WEAVER_LLM_API_KEY",
    "WEAVER_EMBEDDING_PROVIDER",
    "WEAVER_EMBEDDING_URL",
    "WEAVER_EMBEDDING_MODEL",
];

#[derive(Parser, Debug)]
#[command(name = "weaver")]
#[command(
    about = "Architectural control plane MCP server — stores ADRs, decisions, and constraints in SQLite"
)]
struct Args {
    /// Path to a YAML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to the SQLite database file. Will be created if it does not exist.
    #[arg(long, env = "WEAVER_DB")]
    db: Option<PathBuf>,

    /// Run as a persistent Streamable HTTP daemon instead of a stdio subprocess.
    #[arg(long)]
    daemon: bool,

    /// Bind address for daemon mode.
    #[arg(long, env = "WEAVER_BIND")]
    bind: Option<String>,

    /// Repository to index once at startup before serving requests.
    #[arg(long, env = "WEAVER_INDEX_REPO")]
    index_repo: Option<PathBuf>,

    /// Optional source pattern for startup indexing.
    #[arg(long, env = "WEAVER_INDEX_PATTERN")]
    index_pattern: Option<String>,

    /// LLM provider used by record_decision_episode fact extraction.
    #[arg(long, env = "WEAVER_LLM_PROVIDER")]
    llm_provider: Option<String>,

    /// LLM model used by record_decision_episode fact extraction.
    #[arg(long, env = "WEAVER_LLM_MODEL")]
    llm_model: Option<String>,

    /// Base URL for the LLM provider (e.g. http://127.0.0.1:1234 for LM Studio).
    #[arg(long, env = "WEAVER_LLM_URL")]
    llm_url: Option<String>,

    /// API key for the LLM provider (e.g. WEAVER_LLM_API_KEY for LM Studio/OpenAI).
    #[arg(long, env = "WEAVER_LLM_API_KEY")]
    llm_api_key: Option<String>,

    /// Embedding provider: "lmstudio", "ollama", or "openai".
    #[arg(long, env = "WEAVER_EMBEDDING_PROVIDER")]
    embedding_provider: Option<String>,

    /// Base URL for the embedding provider (e.g. http://127.0.0.1:1234 for LM Studio).
    #[arg(long, env = "WEAVER_EMBEDDING_URL")]
    embedding_url: Option<String>,

    /// Model identifier passed to the embedding provider.
    #[arg(long, env = "WEAVER_EMBEDDING_MODEL")]
    embedding_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    db: Option<PathBuf>,
    daemon: Option<bool>,
    bind: Option<String>,
    index_repo: Option<PathBuf>,
    index_pattern: Option<String>,
    projects: Vec<ProjectConfig>,
    llm_provider: Option<String>,
    llm_model: Option<String>,
    llm_url: Option<String>,
    llm_api_key: Option<String>,
    embedding_provider: Option<String>,
    embedding_url: Option<String>,
    embedding_model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    #[serde(alias = "repo", alias = "repo_path")]
    path: PathBuf,
    #[serde(default, alias = "index_pattern")]
    pattern: Option<String>,
}

#[derive(Debug)]
struct IndexJob {
    repo_path: PathBuf,
    pattern: Option<String>,
}

#[derive(Debug)]
struct EffectiveConfig {
    db: PathBuf,
    daemon: bool,
    bind: String,
    index_jobs: Vec<IndexJob>,
    llm_provider: Option<String>,
    llm_model: Option<String>,
    llm_url: Option<String>,
    llm_api_key: Option<String>,
    embedding_provider: Option<String>,
    embedding_url: Option<String>,
    embedding_model: Option<String>,
}

impl EffectiveConfig {
    fn from_sources(args: Args, file: Option<FileConfig>) -> anyhow::Result<Self> {
        let file = file.unwrap_or_default();
        let db = args
            .db
            .or(file.db)
            .ok_or_else(|| anyhow::anyhow!("missing database path: pass --db, set WEAVER_DB, or set db in {DEFAULT_CONFIG_FILE}"))?;

        let mut index_jobs = Vec::new();
        if let Some(repo_path) = args.index_repo.or(file.index_repo) {
            index_jobs.push(IndexJob {
                repo_path,
                pattern: args.index_pattern.or(file.index_pattern),
            });
        }
        index_jobs.extend(file.projects.into_iter().map(|project| IndexJob {
            repo_path: project.path,
            pattern: project.pattern,
        }));

        Ok(Self {
            db,
            daemon: args.daemon || file.daemon.unwrap_or(false),
            bind: args
                .bind
                .or(file.bind)
                .unwrap_or_else(|| DEFAULT_BIND.into()),
            index_jobs,
            llm_provider: args.llm_provider.or(file.llm_provider),
            llm_model: args.llm_model.or(file.llm_model),
            llm_url: args.llm_url.or(file.llm_url),
            llm_api_key: args.llm_api_key.or(file.llm_api_key),
            embedding_provider: args.embedding_provider.or(file.embedding_provider),
            embedding_url: args.embedding_url.or(file.embedding_url),
            embedding_model: args.embedding_model.or(file.embedding_model),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Write tracing output to stderr so stdout is reserved for MCP protocol traffic.
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();

    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let args = Args::parse_from(&raw_args);
    let file_config = config_path_to_load(&args, &raw_args)?
        .map(|path| load_file_config(&path))
        .transpose()?;
    let config = EffectiveConfig::from_sources(args, file_config)?;

    if let Some(provider) = &config.llm_provider {
        std::env::set_var("WEAVER_LLM_PROVIDER", provider);
    }
    if let Some(model) = &config.llm_model {
        std::env::set_var("WEAVER_LLM_MODEL", model);
    }
    if let Some(url) = &config.llm_url {
        std::env::set_var("WEAVER_LLM_URL", url);
    }
    if let Some(key) = &config.llm_api_key {
        std::env::set_var("WEAVER_LLM_API_KEY", key);
    }
    if let Some(p) = &config.embedding_provider {
        std::env::set_var("WEAVER_EMBEDDING_PROVIDER", p);
    }
    if let Some(u) = &config.embedding_url {
        std::env::set_var("WEAVER_EMBEDDING_URL", u);
    }
    if let Some(m) = &config.embedding_model {
        std::env::set_var("WEAVER_EMBEDDING_MODEL", m);
    }

    let db_url = format!("sqlite:{}?mode=rwc", config.db.display());
    let store = storage::SqliteStore::connect(&db_url).await?;
    store.run_migrations().await?;

    for index_job in config.index_jobs {
        // Schedule repository indexing in the background so the daemon can accept
        // incoming MCP connections immediately. Long-running indexing should not
        // block the server from starting.
        let repo_path = index_job.repo_path.to_string_lossy().to_string();
        tracing::info!(repo_path = %repo_path, "scheduling background indexing");
        let store_for_index = Arc::new(store.clone());
        let pattern = index_job.pattern;
        tokio::spawn(async move {
            tracing::info!(repo_path = %repo_path, "background indexing started");
            match tools::ingest_symbols::run(
                &store_for_index,
                tools::ingest_symbols::IngestSymbolsParams {
                    repo_path: repo_path.clone(),
                    pattern,
                    force: false,
                },
                None,
                None,
            )
            .await
            {
                Ok(response) => tracing::info!(
                    repo_path = %repo_path,
                    answer = response.response.answer.as_deref().unwrap_or(""),
                    warnings = ?response.response.warnings,
                    "background indexing complete"
                ),
                Err(e) => {
                    tracing::error!(repo_path = %repo_path, error = %e, "background indexing failed")
                }
            }
        });
    }

    if config.daemon {
        let bind: std::net::SocketAddr = config.bind.parse()?;
        daemon::run(store, daemon::DaemonConfig { bind }).await?;
    } else {
        tracing::info!("weaver ready");
        use rmcp::ServiceExt;
        let srv = server::ArchServer::new(store);
        let service = srv.serve((tokio::io::stdin(), tokio::io::stdout())).await?;
        service.waiting().await?;
    }

    Ok(())
}

fn config_path_to_load(args: &Args, raw_args: &[OsString]) -> anyhow::Result<Option<PathBuf>> {
    if let Some(path) = &args.config {
        return Ok(Some(path.clone()));
    }

    if has_cli_config_args(raw_args)
        || CONFIG_ENV_VARS
            .iter()
            .any(|name| std::env::var_os(name).is_some())
    {
        return Ok(None);
    }

    let default_path = std::env::current_dir()?.join(DEFAULT_CONFIG_FILE);
    Ok(default_path.exists().then_some(default_path))
}

fn has_cli_config_args(raw_args: &[OsString]) -> bool {
    raw_args.iter().skip(1).any(|arg| {
        arg.to_str().is_some_and(|arg| {
            matches!(
                arg,
                "--config"
                    | "--db"
                    | "--daemon"
                    | "--bind"
                    | "--index-repo"
                    | "--index-pattern"
                    | "--llm-provider"
                    | "--llm-model"
                    | "--llm-url"
                    | "--llm-api-key"
                    | "--embedding-provider"
                    | "--embedding-url"
                    | "--embedding-model"
            ) || arg.starts_with("--config=")
                || arg.starts_with("--db=")
                || arg.starts_with("--bind=")
                || arg.starts_with("--index-repo=")
                || arg.starts_with("--index-pattern=")
                || arg.starts_with("--llm-provider=")
                || arg.starts_with("--llm-model=")
                || arg.starts_with("--llm-url=")
                || arg.starts_with("--llm-api-key=")
                || arg.starts_with("--embedding-provider=")
                || arg.starts_with("--embedding-url=")
                || arg.starts_with("--embedding-model=")
        })
    })
}

fn load_file_config(path: &Path) -> anyhow::Result<FileConfig> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let config_text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
    let mut config: FileConfig = serde_yaml::from_str(&config_text)
        .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))?;
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    config.resolve_relative_paths(&base_dir);
    Ok(config)
}

impl FileConfig {
    fn resolve_relative_paths(&mut self, base_dir: &Path) {
        resolve_path(&mut self.db, base_dir);
        resolve_path(&mut self.index_repo, base_dir);
        for project in &mut self.projects {
            if !project.path.is_absolute() {
                project.path = base_dir.join(&project.path);
            }
        }
    }
}

fn resolve_path(path: &mut Option<PathBuf>, base_dir: &Path) {
    if let Some(path) = path {
        if !path.is_absolute() {
            *path = base_dir.join(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            config: None,
            db: None,
            daemon: false,
            bind: None,
            index_repo: None,
            index_pattern: None,
            llm_provider: None,
            llm_model: None,
            llm_url: None,
            llm_api_key: None,
            embedding_provider: None,
            embedding_url: None,
            embedding_model: None,
        }
    }

    #[test]
    fn file_config_supplies_daemon_settings_and_projects() {
        let config = FileConfig {
            db: Some(PathBuf::from("memory.db")),
            daemon: Some(true),
            bind: Some("127.0.0.1:8444".into()),
            projects: vec![ProjectConfig {
                path: PathBuf::from("repo-a"),
                pattern: Some("src/**/*.rs".into()),
            }],
            ..Default::default()
        };

        let effective = EffectiveConfig::from_sources(args(), Some(config)).unwrap();

        assert_eq!(effective.db, PathBuf::from("memory.db"));
        assert!(effective.daemon);
        assert_eq!(effective.bind, "127.0.0.1:8444");
        assert_eq!(effective.index_jobs.len(), 1);
        assert_eq!(effective.index_jobs[0].repo_path, PathBuf::from("repo-a"));
        assert_eq!(
            effective.index_jobs[0].pattern.as_deref(),
            Some("src/**/*.rs")
        );
    }

    #[test]
    fn cli_values_override_file_config() {
        let mut parsed_args = args();
        parsed_args.db = Some(PathBuf::from("cli.db"));
        parsed_args.bind = Some("127.0.0.1:9000".into());
        parsed_args.llm_model = Some("cli-model".into());

        let config = FileConfig {
            db: Some(PathBuf::from("file.db")),
            bind: Some("127.0.0.1:8444".into()),
            llm_model: Some("file-model".into()),
            ..Default::default()
        };

        let effective = EffectiveConfig::from_sources(parsed_args, Some(config)).unwrap();

        assert_eq!(effective.db, PathBuf::from("cli.db"));
        assert_eq!(effective.bind, "127.0.0.1:9000");
        assert_eq!(effective.llm_model.as_deref(), Some("cli-model"));
    }
}
