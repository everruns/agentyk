//! Crate error type.

/// Errors surfaced by agentyk.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The agent was composed without a model, or is otherwise invalid.
    #[error("invalid agent: {0}")]
    InvalidAgent(String),

    /// No driver registered for the model's `DriverId`.
    #[error("no chat driver registered for `{0}`")]
    UnknownDriver(String),

    /// A turn hit the iteration ceiling without producing a final response.
    #[error("turn exceeded max iterations ({0})")]
    MaxIterations(usize),

    /// The turn was cancelled via a [`crate::cancellation::CancellationToken`].
    #[error("turn cancelled")]
    Cancelled,

    /// An LLM driver failed.
    #[error("driver error: {0}")]
    Driver(String),

    /// An MCP server or transport failed.
    #[error("mcp error: {0}")]
    Mcp(String),

    /// The event log failed to append or read.
    #[error("event log error: {0}")]
    EventLog(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
