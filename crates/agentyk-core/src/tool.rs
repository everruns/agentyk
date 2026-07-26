//! Tools — functions the model can call during a turn.
//!
//! Mirrors the everruns `Tool` / `ToolDefinition` vocabulary. Tools reach the
//! model through capabilities ([`crate::capability::Capability::tools`]) or
//! directly via `AgentBuilder::tool` in the `agentyk` framework crate.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::id::{SessionId, TurnId};

/// What the model sees: name, description, and a JSON-schema for arguments.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolDefinition {
    /// What the model calls it. Unique within one turn's tool list.
    pub name: String,
    /// What it does, written for the model — this is prompt text, and the
    /// main lever on whether the tool gets called correctly.
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: serde_json::Value,
    /// Generic, serializable extension hatch — the tool-definition analogue
    /// of [`crate::event::EventData::Custom`]. Host-side, everruns-flavored
    /// metadata an adopting host owns (a risk/hint taxonomy like
    /// `readonly`/`destructive`/`open_world`, categories, MCP annotations)
    /// rides here rather than growing typed core fields; it is **not** sent
    /// to the model. `Null` for the common case.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl ToolDefinition {
    /// Describe a tool to the model.
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
/// Host-side policy about a tool — how it is offered, not what it does.
#[non_exhaustive]
pub struct ToolPolicy {
    /// Whether the model is told about this tool up front.
    pub deferrable: DeferrablePolicy,
}

impl ToolPolicy {
    /// A policy with an explicit deferrability.
    pub fn new(deferrable: DeferrablePolicy) -> Self {
        Self { deferrable }
    }

    /// Hide this tool from the model's tool list while keeping it callable —
    /// see [`DeferrablePolicy::Deferred`].
    pub fn deferred() -> Self {
        Self::new(DeferrablePolicy::Deferred)
    }
}

/// The result of executing a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolOutput {
    /// What the model receives back.
    pub content: String,
    /// Whether `content` describes a failure. A tool failure is a *result*,
    /// not a turn failure: the model sees it and gets to react.
    pub is_error: bool,
    /// Structured result data for the **host**, never sent to the model —
    /// the tool-result analogue of [`ToolDefinition::metadata`].
    ///
    /// A tool result is one string because that is what every provider's
    /// wire format accepts. That is fine for the model and lossy for
    /// everything else: a UI that wants to render a diff, a file list, an
    /// exit code, or a preview has to re-parse prose. Put the structured
    /// form here and the flat form in `content`, and both consumers get what
    /// they need. It rides on `tool.completed`, so listeners see it live and
    /// replay sees it again.
    ///
    /// `Null` for the common case, and omitted from serialization then, so
    /// existing logs still deserialize.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl ToolOutput {
    /// Build an output whose error-ness is only known at runtime; prefer
    /// [`ToolOutput::text`] or [`ToolOutput::error`] when it is known.
    pub fn new(content: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: content.into(),
            is_error,
            metadata: serde_json::Value::Null,
        }
    }

    /// A successful result.
    pub fn text(content: impl Into<String>) -> Self {
        Self::new(content, false)
    }

    /// A failure the model should see and can react to. Prefer this over
    /// failing the turn: "file not found" is something a model can recover
    /// from.
    pub fn error(content: impl Into<String>) -> Self {
        Self::new(content, true)
    }

    /// Attach structured data for the host — see [`ToolOutput::metadata`].
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// One progress report from a tool that is still running.
///
/// Tools that take real time — a build, a test run, a large fetch — are
/// otherwise silent between `tool.started` and `tool.completed`, which is the
/// exact window a user is waiting through. A report is **ephemeral**: it
/// reaches listeners and is never persisted, the same contract as a streaming
/// message delta.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolProgress {
    /// One line for a human, e.g. `"compiling agentyk-core"`.
    pub message: String,
    /// Optional structured detail for a host that renders more than a line —
    /// a percentage, a byte count, a chunk of streamed output. Host-defined;
    /// core never interprets it.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

impl ToolProgress {
    /// A plain progress line.
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: serde_json::Value::Null,
        }
    }

    /// Attach structured detail — see [`ToolProgress::detail`].
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

/// Where a running tool's progress reports go.
///
/// Implemented by the host, not by tools: the in-process executor's
/// implementation turns each report into an ephemeral `tool.progress` event
/// for the session's listeners. A durable host might instead forward them to
/// a queue. Tools never construct one — they call
/// [`ToolContext::report_progress`], which does nothing when no host supplied
/// a sink (a unit test, say).
#[async_trait]
pub trait ToolProgressSink: Send + Sync {
    /// Deliver one report. Must not fail the tool: a host that cannot
    /// deliver drops the report.
    async fn progress(&self, progress: ToolProgress);
}

/// Execution-time context handed to a tool.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct ToolContext {
    /// The session this call belongs to — the scope for anything the tool
    /// keeps between turns.
    pub session_id: SessionId,
    /// The turn this call belongs to.
    pub turn_id: TurnId,
    /// Host-injected services a tool can downcast by type — see
    /// [`crate::extensions::Extensions`]. Empty unless the agent's builder
    /// set some.
    pub extensions: crate::extensions::Extensions,
    /// The turn's cancellation signal.
    ///
    /// A host already drops a tool's future when the turn is cancelled, so a
    /// tool needs this only to do something *better* than being dropped —
    /// flush a partial result, remove a temp file, report what it got done.
    /// A tool that ignores it is still cancellable.
    ///
    /// Middleware reaches it the same way (via
    /// [`crate::middleware::ToolInvocation::context`]), which is how an
    /// approval prompt stops waiting when the turn is cancelled.
    pub cancellation: crate::cancellation::CancellationToken,
    /// Where [`ToolContext::report_progress`] sends its reports. `None` when
    /// no host is listening.
    pub progress: Option<Arc<dyn ToolProgressSink>>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("cancellation", &self.cancellation)
            .field("progress", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// Context for a call in this session and turn: no extensions, no
    /// progress sink, and a token nobody else holds (so nothing cancels it).
    pub fn new(session_id: SessionId, turn_id: TurnId) -> Self {
        Self {
            session_id,
            turn_id,
            extensions: crate::extensions::Extensions::new(),
            cancellation: crate::cancellation::CancellationToken::new(),
            progress: None,
        }
    }

    /// Make the host's injected services reachable from this call.
    pub fn with_extensions(mut self, extensions: crate::extensions::Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Share the turn's cancellation signal with this call.
    pub fn with_cancellation(
        mut self,
        cancellation: crate::cancellation::CancellationToken,
    ) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Route this call's progress reports to the host.
    pub fn with_progress(mut self, progress: Arc<dyn ToolProgressSink>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Report progress, if anyone is listening.
    ///
    /// Deliberately infallible and silent when unwired: a tool should be
    /// able to narrate what it is doing without branching on whether a host
    /// cares.
    ///
    /// ```
    /// # use agentyk_core::{ToolContext, ToolOutput, ToolProgress};
    /// # async fn run(context: &ToolContext) -> ToolOutput {
    /// context.report_progress(ToolProgress::message("running tests")).await;
    /// ToolOutput::text("ok")
    /// # }
    /// ```
    pub async fn report_progress(&self, progress: ToolProgress) {
        if let Some(sink) = &self.progress {
            sink.progress(progress).await;
        }
    }
}

/// A function the model can call.
///
/// Tools reach the model through a [`crate::capability::Capability`], or
/// directly via `AgentBuilder::tool`. For a closure, use [`FnTool`].
#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model is told about this tool.
    fn definition(&self) -> ToolDefinition;

    /// Run it.
    ///
    /// Returns a [`ToolOutput`] rather than a `Result` on purpose: a tool
    /// that fails should tell the *model* so, and let it recover, instead of
    /// failing the turn. `arguments` come from the model — validate them.
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
    /// Wrap a closure as a tool. `parameters` is the JSON schema the model
    /// sees for the arguments.
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
