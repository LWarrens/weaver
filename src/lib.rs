//! Weaver — architectural control plane MCP server.
//!
//! Library crate exposing the storage, tool, and provider layers so that
//! integration tests (`tests/`) and future embedders can drive the same code
//! paths as the `weaver` binary.

pub mod adapters;
pub mod daemon;
pub mod domain;
pub mod embeddings;
pub mod error;
pub mod llm;
pub mod server;
pub mod storage;
pub mod tools;

/// Process-wide lock for tests that mutate environment variables.
///
/// `std::env::set_var` / `remove_var` are process-global, so any two tests that
/// touch the same `WEAVER_*` variable must not run concurrently — across module
/// boundaries, not just within one file. Every such test takes this lock.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
