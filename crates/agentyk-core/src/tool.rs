//! Tools — functions the model can call during a turn.
//!
//! Mirrors the everruns `Tool` / `ToolDefinition` vocabulary. Tools reach the
//! model through capabilities ([`crate::capability::Capability::tools`]) or
//! directly via `AgentBuilder::tool` in the `agentyk` framework crate.

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::id::{SessionId, TurnId};

/// What the model sees: name, description, and a JSON-schema for arguments.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: serde_json::Value,
    /// Generic, serializable extension hatch — the tool-definition analogue
    /// of [`crate::event::EventData::Custom`]. Host-side, everruns-flavored
    /// metadata a satellite crate owns (a risk/hint taxonomy like
    /// `readonly`/`destructive`/`open_world`, categories, MCP annotations)
    /// rides here rather than growing typed core fields; it is **not** sent
    /// to the model. `Null` for the common case.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            metadata: serde_json::Value::Null,
        }
    }

    /// Attach host-side extension metadata — see [`ToolDefinition::metadata`].
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Whether a tool's definition is sent to the model up front. Mirrors
/// everruns' `DeferrablePolicy` (what ToolSearch-style deferred-tool
/// discovery is built on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeferrablePolicy {
    /// Always included in the tool list sent to the model.
    #[default]
    Never,
    /// Omitted from `atoms::assemble`'s tool list — a capability can still
    /// execute it (it stays in the lookup table), it just isn't offered to
    /// the model by default. Surfacing deferred tools on demand (a
    /// ToolSearch-style capability) is not implemented; this is the policy
    /// slot such a capability would read.
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolPolicy {
    pub deferrable: DeferrablePolicy,
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
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    /// Host-injected services a tool can downcast by type — see
    /// [`crate::extensions::Extensions`]. Empty unless the agent's builder
    /// set some.
    pub extensions: crate::extensions::Extensions,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    async fn execute(&self, arguments: serde_json::Value, context: &ToolContext) -> ToolOutput;

    /// Default: always offered to the model — see [`ToolPolicy`].
    fn policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }
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
            definition: ToolDefinition::new(name, description, parameters),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_hatch_round_trips_and_is_omitted_when_null() {
        let plain = ToolDefinition::new("read", "Read a file.", json!({"type": "object"}));
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(!plain_json.contains("metadata"));

        let hinted = plain.with_metadata(json!({"hints": {"risk": "readonly"}}));
        let back: ToolDefinition =
            serde_json::from_str(&serde_json::to_string(&hinted).unwrap()).unwrap();
        assert_eq!(back, hinted);
        assert_eq!(back.metadata["hints"]["risk"], "readonly");
    }
}
