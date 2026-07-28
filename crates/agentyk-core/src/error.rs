//! Crate error type.

use std::fmt;

/// Coarse classification of an LLM driver failure, so a host (in particular
/// a durable executor retrying activities) can tell a transient failure
/// worth retrying apart from a terminal one that will never succeed by
/// itself. Mirrors the intent of everruns' `LlmErrorKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LlmErrorKind {
    /// The provider rejected the request for being over its rate limit
    /// (HTTP 429). Retry after a backoff.
    RateLimited,
    /// The provider is temporarily overloaded (HTTP 503, Anthropic 529).
    /// Retry after a backoff.
    Overloaded,
    /// The request timed out before a response arrived. Retry.
    Timeout,
    /// A transport-level failure (connection refused, DNS, TLS, a stream
    /// read that dropped mid-response). Retry.
    Network,
    /// Generic server-side failure (HTTP 5xx not otherwise classified).
    /// Usually worth one retry.
    ServerError,
    /// Missing or rejected credentials (HTTP 401/403, or no api key was
    /// configured at all). Will never succeed without a config change —
    /// do not retry.
    Authentication,
    /// The request itself was malformed (HTTP 400/404/422 or similar).
    /// Will never succeed unchanged — do not retry.
    InvalidRequest,
    /// Unclassified — an unexpected response shape, a decode failure, or a
    /// status code with no clear category. Conservative default: do not
    /// retry blindly.
    Unknown,
}

impl LlmErrorKind {
    /// Whether a host should consider retrying the request after a backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmErrorKind::RateLimited
                | LlmErrorKind::Overloaded
                | LlmErrorKind::Timeout
                | LlmErrorKind::Network
                | LlmErrorKind::ServerError
        )
    }
}

impl fmt::Display for LlmErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            LlmErrorKind::RateLimited => "rate_limited",
            LlmErrorKind::Overloaded => "overloaded",
            LlmErrorKind::Timeout => "timeout",
            LlmErrorKind::Network => "network",
            LlmErrorKind::ServerError => "server_error",
            LlmErrorKind::Authentication => "authentication",
            LlmErrorKind::InvalidRequest => "invalid_request",
            LlmErrorKind::Unknown => "unknown",
        };
        f.write_str(label)
    }
}

/// Errors surfaced by agentyk.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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

    /// The turn was deliberately sealed — see [`crate::turn::SealReason`].
    #[error("turn sealed: {0:?}")]
    Sealed(crate::turn::SealReason),

    /// A blockable user hook refused the prompt or tool action.
    #[error("hook blocked execution: {reason}")]
    HookBlocked {
        /// Diagnostic reason supplied by the hook.
        reason: String,
        /// Optional message suitable for a user-facing surface.
        user_message: Option<String>,
    },

    /// An LLM driver failed. `kind` classifies whether retrying is worth it
    /// — see [`Error::is_retryable`].
    #[error("driver error ({kind}): {message}")]
    Driver {
        /// Whether retrying is worth it.
        kind: LlmErrorKind,
        /// What the provider or transport reported.
        message: String,
    },

    /// An MCP server or transport failed.
    #[error("mcp error: {0}")]
    Mcp(String),

    /// The event log failed to append or read.
    #[error("event log error: {0}")]
    EventLog(String),

    /// An append expected a different end-of-stream sequence.
    #[error("event stream conflict: expected version {expected}, found {actual}")]
    EventConflict {
        /// Version supplied by the writer.
        expected: u64,
        /// Version currently stored.
        actual: u64,
    },

    /// Local I/O failed — an event log file, a workspace read.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Serialization or deserialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Anything without a more specific variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Shorthand for `Error::Driver { kind, message: message.into() }`.
    pub fn driver(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        Error::Driver {
            kind,
            message: message.into(),
        }
    }

    /// Whether a host should consider retrying the operation that produced
    /// this error. Only [`Error::Driver`] carries enough information to say
    /// yes; every other variant is a local/config problem retrying can't fix.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Driver { kind, .. } => kind.is_retryable(),
            _ => false,
        }
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_kinds_match_the_documented_set() {
        assert!(LlmErrorKind::RateLimited.is_retryable());
        assert!(LlmErrorKind::Overloaded.is_retryable());
        assert!(LlmErrorKind::Timeout.is_retryable());
        assert!(LlmErrorKind::Network.is_retryable());
        assert!(LlmErrorKind::ServerError.is_retryable());
        assert!(!LlmErrorKind::Authentication.is_retryable());
        assert!(!LlmErrorKind::InvalidRequest.is_retryable());
        assert!(!LlmErrorKind::Unknown.is_retryable());
    }

    #[test]
    fn error_is_retryable_delegates_to_driver_kind_only() {
        assert!(Error::driver(LlmErrorKind::RateLimited, "429").is_retryable());
        assert!(!Error::driver(LlmErrorKind::Authentication, "401").is_retryable());
        assert!(!Error::Cancelled.is_retryable());
        assert!(!Error::MaxIterations(4).is_retryable());
    }
}
