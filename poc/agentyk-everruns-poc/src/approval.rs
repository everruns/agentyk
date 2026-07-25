//! Everruns-style approval expressed as ordinary engine middleware.

use std::sync::Arc;

use agentyk_core::message::ToolCall;
use agentyk_core::middleware::{ToolCallDecision, ToolInvocation, TurnMiddleware};
use async_trait::async_trait;

use crate::hints::ToolHints;

/// What an [`Approver`] decides for a risky tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Let the call run.
    Allow,
    /// Block it with a user-facing reason.
    Deny {
        /// Message returned to the model and user.
        user_message: String,
    },
}

/// Consulted before a risky tool call runs.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Decide whether one risky call may run.
    async fn approve(&self, call: &ToolCall, hints: &ToolHints) -> ApprovalDecision;
}

/// An approver that permits everything.
pub struct AllowAll;

#[async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Adapts an [`Approver`] into canonical turn middleware.
pub struct ApprovalMiddleware(Arc<dyn Approver>);

impl ApprovalMiddleware {
    /// Gate risky tools through this approver.
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self(Arc::new(approver))
    }
}

#[async_trait]
impl TurnMiddleware for ApprovalMiddleware {
    fn name(&self) -> &str {
        "everruns-approval"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        let hints = invocation
            .definition
            .and_then(ToolHints::from_definition)
            .unwrap_or_default();
        if !hints.needs_approval() {
            return ToolCallDecision::Proceed;
        }
        match self.0.approve(invocation.call, &hints).await {
            ApprovalDecision::Allow => ToolCallDecision::Proceed,
            ApprovalDecision::Deny { user_message } => ToolCallDecision::Deny {
                reason: user_message,
            },
        }
    }
}
