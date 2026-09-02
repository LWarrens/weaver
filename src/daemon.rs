use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::server::{ArchServer, ReloadHandle};
use crate::storage::SqliteStore;

pub struct DaemonConfig {
    pub bind: SocketAddr,
}

/// Run the HTTP MCP daemon, restarting on SIGHUP / `reload_daemon` tool call and
/// stopping on SIGTERM / Ctrl-C.
///
/// The `TcpListener` is bound **once** before the loop. Each generation clones the
/// underlying socket FD so the OS always has at least one open handle to the port.
/// Connections that arrive during the brief inter-generation window queue in the
/// kernel accept-backlog instead of getting ECONNREFUSED.
///
/// The `SqliteStore` connection pool also persists across reloads; only the HTTP
/// layer and `ArchServer` are rebuilt.
pub async fn run(store: SqliteStore, config: DaemonConfig) -> anyhow::Result<()> {
    // Bind once via tokio so socket options (SO_REUSEADDR, SO_EXCLUSIVEADDRUSE on
    // Windows) match what tokio sets. Convert to std immediately so we own a plain
    // fd that can be try_clone()'d across reload generations.
    let std_listener = tokio::net::TcpListener::bind(config.bind).await?.into_std()?;
    std_listener.set_nonblocking(true)?;

    tracing::info!(
        bind = %config.bind,
        endpoint = %format!("http://{}/mcp", config.bind),
        "weaver streamable HTTP daemon listening"
    );

    loop {
        let stop = CancellationToken::new();
        let reload_flag = Arc::new(AtomicBool::new(false));

        tokio::spawn(wait_for_signal(stop.clone(), reload_flag.clone()));

        let reload_handle = ReloadHandle::new(stop.clone(), reload_flag.clone());
        let srv = ArchServer::with_reload(store.clone(), Some(reload_handle));

        // Clone the socket FD for this generation. When serve_once returns and
        // the clone is dropped, std_listener still holds the port open.
        let listener = tokio::net::TcpListener::from_std(std_listener.try_clone()?)?;

        serve_once(srv, listener, stop).await?;

        if !reload_flag.load(Ordering::Relaxed) {
            break;
        }
        tracing::info!("hot-reloading daemon");
    }
    Ok(())
}

/// Serve one generation of the streamable-HTTP MCP service until `stop` fires.
async fn serve_once(
    srv: ArchServer,
    listener: tokio::net::TcpListener,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use tower_http::cors::{Any, CorsLayer};

    // Default keep_alive is 300 s, which triggers a fatal worker error in rmcp 1.6.0.
    // 30 minutes is safe; the client pings every 2 min so active sessions never hit it.
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = Some(std::time::Duration::from_secs(30 * 60));
    let session_manager = Arc::new(session_manager);

    let service: StreamableHttpService<ArchServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(srv.clone()),
            session_manager,
            StreamableHttpServerConfig::default().with_cancellation_token(stop.child_token()),
        );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(cors);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await?;

    Ok(())
}

/// On Unix: SIGHUP triggers a hot reload; SIGTERM / Ctrl-C trigger a clean shutdown.
/// On Windows: only Ctrl-C is handled (no SIGHUP equivalent).
#[cfg(unix)]
async fn wait_for_signal(stop: CancellationToken, reload_flag: Arc<AtomicBool>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sighup = signal(SignalKind::hangup()).expect("SIGHUP listener");
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM listener");

    tokio::select! {
        _ = sighup.recv() => {
            tracing::info!("SIGHUP received — triggering hot reload");
            reload_flag.store(true, Ordering::Relaxed);
            stop.cancel();
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received — shutting down");
            stop.cancel();
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received — shutting down");
            stop.cancel();
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal(stop: CancellationToken, _reload_flag: Arc<AtomicBool>) {
    // If the process has no console (e.g. spawned by VS Code without one),
    // ctrl_c() fails immediately. Don't cancel the stop token in that case —
    // the server should keep running and be killed externally.
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            tracing::info!("Ctrl+C received — shutting down");
            stop.cancel();
        }
        Err(e) => {
            tracing::warn!(error = %e, "ctrl_c signal handler failed — server will run until killed");
        }
    }
}
