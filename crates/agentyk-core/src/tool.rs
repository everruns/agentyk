//! Tools — functions the model can call during a turn.
//!
//! Mirrors the everruns `Tool` / `ToolDefinition` vocabulary. Tools reach the
//! model through capabilities ([`crate::capability::Capability::tools`]) or
//! directly via [`crate::agent::AgentBuilder::tool`].

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::id::{SessionId, TurnId};

/// What the model sees: name, description, and a JSON-schema for arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: serde_json::Value,
}

/// The result of executing a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Execution-time context handed to a tool.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    async fn execute(&self, arguments: serde_json::Value, context: &ToolContext) -> ToolOutput;
}

type BoxedToolFuture = Pin<Box<dyn Future<Output = ToolOutput> + Send>>;
type BoxedToolFn = Box<dyn Fn(serde_json::Value) -> BoxedToolFuture + Send + Sync>;

/// A tool built from a closure — the quickest way to hand the model a
/// function.
///
/// ```
/// use agentyk_core::{FnTool, ToolOutput};
/// use serde_json::json;
///
/// let add = FnTool::new(
///     "add",
///     "Add two numbers.",
///     json!({
///         "type": "object",
///         "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
///         "required": ["a", "b"]
///     }),
///     |args| async move {
///         let a = args["a"].as_f64().unwrap_or(0.0);
///         let b = args["b"].as_f64().unwrap_or(0.0);
///         ToolOutput::text((a + b).to_string())
///     },
/// );
/// ```
pub struct FnTool {
    definition: ToolDefinition,
    handler: BoxedToolFn,
}

impl FnTool {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: F,
    ) -> Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolOutput> + Send + 'static,
    {
        Self {
            definition: ToolDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
            handler: Box::new(move |args| Box::pin(handler(args))),
        }
    }
}

#[async_trait]
impl Tool for FnTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        (self.handler)(arguments).await
    }
}
