#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("invalid input — {field}: {reason}")]
    InvalidInput { field: &'static str, reason: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
