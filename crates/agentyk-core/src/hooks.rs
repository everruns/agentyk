//! Act hooks — interception points around tool execution.
//!
//! Adopted from everruns' act pipeline (`PreToolUseHook`/`PostToolExecHook`):
//! [`PreToolUseHook`] runs before a tool executes and can deny it (the seam
//! approval gating and guardrails are built on); [`PostToolExecHook`] runs
//! after and can transform the result (e.g. truncating oversized output).
//! Hooks are attached to the agent (`AgentBuilder::pre_tool_hook` /
//! `.post_tool_hook`) and orchestrated by the executor around
//! [`crate::atoms::act`] — they are host policy, not a third atom.

use async_trait::async_trait;

use crate::message::ToolCall;
use crate::tool::{ToolContext, ToolOutput};

/// Whether a tool call may proceed.
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolUseDecision {
    Allow,
    /// Deny with a reason. The tool never runs; the reason becomes the
    /// (error) result the model sees, and a `tool.denied` event is
    /// recorded alongside the usual `tool.started`/`tool.completed` pair.
    Deny {
        reason: String,
    },
}

#[async_trait]
pub trait PreToolUseHook: Send + Sync {
    async fn before_tool_use(&self, call: &ToolCall, context: &ToolContext) -> PreToolUseDecision;
}

#[async_trait]
pub trait PostToolExecHook: Send + Sync {
    /// Observe or transform a tool's result. Runs for every executed call
    /// (including ones the tool itself reported as an error) — denied calls
    /// never reach here.
    async fn after_tool_exec(
        &self,
        call: &ToolCall,
        output: ToolOutput,
        context: &ToolContext,
    ) -> ToolOutput;
}
