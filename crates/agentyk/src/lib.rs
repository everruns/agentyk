//! # agentyk
//!
//! Compose agents from values and run them.
//!
//! An [`Agent`] is built entirely by value — a system prompt, a model
//! ([`ModelSpec`]), capabilities ([`Capability`]), tools ([`Tool`]) — with no
//! entity creation, no registration, and no ids to thread. Ids exist only as
//! internal correlation handles on sessions and events.
//!
//! This is the **facade crate**: builders, the canonical engine and its
//! in-process host, bundled drivers (sim; OpenAI/Anthropic behind `http`), the
//! JSONL event store, filesystem support, and MCP support. The portable
//! contract — traits, the event protocol, and the turn state machine — lives in
//! [`agentyk-core`](https://crates.io/crates/agentyk-core) and is fully
//! re-exported here; depend on core directly only when *implementing* a seam
//! (a custom driver, capability, or event store).
//!
//! The domain language (events protocol, capabilities, drivers, the
//! `input → reason → act` turn) is inherited from
//! [everruns](https://github.com/everruns/everruns); agentyk is the
//! value-first core that everruns-core and everruns-runtime are intended to
//! be rebuilt on top of.
//!
//! # Where to start
//!
//! - Run the deterministic example below to learn the turn loop.
//! - See the repository's `production_agent` example to compose a typed tool,
//!   a safety hook, [`JsonlEventLog`], multiple turns, and replay-based resume.
//! - Add [`FileSystemCapability`] for workspace-scoped file tools or
//!   [`McpCapability`] for tools discovered from an MCP server.
//! - Replace [`SimDriver`] with [`OpenAiDriver`] or [`AnthropicDriver`] when
//!   the host is ready to supply provider credentials.
//!
//! # Quick start
//!
//! ```
//! use agentyk::{Agent, ModelSpec, SimDriver, SimTurn};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> agentyk::Result<()> {
//! let agent = Agent::builder()
//!     .name("greeter")
//!     .system_prompt("You are terse.")
//!     .model(ModelSpec::llmsim())
//!     .driver(SimDriver::new([SimTurn::text("hi!")]))
//!     .build()?;
//!
//! let mut session = agent.session();
//! let turn = session.run("hello").await?;
//! assert_eq!(turn.response, "hi!");
//! assert!(!session.events().await?.is_empty());
//! # Ok(())
//! # }
//! ```
//!
//! With a real provider (feature `http`):
//!
//! ```no_run
//! # #[cfg(feature = "http")]
//! # async fn run() -> agentyk::Result<()> {
//! use agentyk::{Agent, AnthropicDriver, ModelSpec};
//!
//! let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");
//! let agent = Agent::builder()
//!     .system_prompt("You are a coding agent.")
//!     .model(ModelSpec::anthropic("claude-sonnet-4-5").api_key(key))
//!     .driver(AnthropicDriver::new())
//!     .build()?;
//!
//! let result = agent.run("List the files in the current directory.").await?;
//! println!("{}", result.response);
//! # Ok(())
//! # }
//! ```

// Enables the "Available on crate feature ..." badges when docs.rs builds
// with `--cfg docsrs` on nightly; a no-op for every ordinary build.
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod drivers;
#[cfg(feature = "fs")]
#[cfg_attr(docsrs, doc(cfg(feature = "fs")))]
pub mod filesystem;
#[cfg(feature = "hooks")]
#[cfg_attr(docsrs, doc(cfg(feature = "hooks")))]
pub mod hooks;
pub mod jsonl_log;
#[cfg(feature = "mcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp")))]
pub mod mcp;

// The full contract: protocol data, seams, the turn machine, atoms, replay.
//
// Listed rather than globbed. A glob would mean every future item in core
// silently appears here too — including ones that collide with a name this
// crate already owns, where the collision surfaces as a confusing error in
// *someone else's* build. Re-exporting explicitly makes widening this crate's
// surface a decision.
pub use agentyk_core::{
    atoms, budget, cancellation, capability, concurrency, context, controls, driver, error, event,
    event_log, extensions, hook, id, input, message, middleware, profile, replay, tool, turn,
};

pub use agentyk_core::{
    BudgetChecker, BudgetDecision, CancellationToken, Capability, ChatDriver, ChatRequest,
    ChatResponse, CommandContext, CommandDescriptor, CompositeEventListener, ContentPart,
    ContextAssembler, ContextAssembly, ContextEvent, ContextRequest, ContextSource,
    DeferrablePolicy, DeltaSink, DriverId, DriverRegistry, Error, Event, EventData, EventId,
    EventListener, EventLog, EventPage, EventRange, EventRequest, EventStore, ExpectedVersion,
    Extensions, ExternalActor, FnTool, History, Hook, HookContext, HookErrorPolicy, HookEvent,
    HookMatcher, HookOutcome, HookPayload, ImageContentPart, InMemoryEventLog,
    InMemoryModelCatalog, InMemorySnapshotStore, InputQueue, LlmErrorKind, Message, MessageId,
    ModelCatalog, ModelProfile, ModelSpec, NoopEventListener, PassthroughContextAssembler,
    ProjectionSnapshot, ReasoningConfig, Result, Role, SealReason, SessionId, SessionPoint,
    SnapshotStore, SystemPromptContext, TextContentPart, Tool, ToolCall, ToolCallDecision,
    ToolChainOutcome, ToolContext, ToolDefinition, ToolEventPresentation, ToolInvocation,
    ToolNarrationContext, ToolNarrationPhase, ToolOutput, ToolPolicy, ToolProgress,
    ToolProgressSink, TurnAction, TurnControls, TurnId, TurnMiddleware, TurnOutcome, TurnPhase,
    TurnState, Usage, after_tool_chain, before_tool_chain, event_types, messages_from_events,
    notify_event_listeners,
};
/// The names most applications want in scope at once.
///
/// ```
/// use agentyk::prelude::*;
///
/// # fn demo() -> Result<Agent> {
/// Agent::builder()
///     .system_prompt("You are terse.")
///     .model(ModelSpec::llmsim())
///     .build()
/// # }
/// ```
pub mod prelude {
    pub use crate::{
        Agent, AgentBuilder, Capability, ChatDriver, CompositeEventListener, Event, EventData,
        EventListener, EventLog, FnTool, Message, ModelSpec, NoopEventListener, Result, RunOptions,
        Session, SessionView, Tool, ToolCallDecision, ToolContext, ToolDefinition, ToolInvocation,
        ToolNarrationContext, ToolNarrationPhase, ToolOutput, TurnMiddleware, TurnResult,
    };
}

pub use agentyk_engine::{
    Agent, AgentBuilder, AgentDefinition, AgentEnvironment, HookRejection, InProcessExecutor,
    PreparedStep, PreparedToolCall, PreparedTurnStart, RunOptions, Session, SessionView,
    TurnEngine, TurnHost, TurnOperation, TurnResult, agent, engine, host, in_process, session,
};
pub use agentyk_macros::tool;
pub use schemars::JsonSchema;

/// Implementation details used by exported macros. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use async_trait::async_trait;
    pub use schemars;
    pub use serde_json;
}
pub use drivers::sim::{SimDriver, SimToolCall, SimTurn};
#[cfg(feature = "fs")]
#[cfg_attr(docsrs, doc(cfg(feature = "fs")))]
pub use filesystem::{
    FileEntry, FileStat, FileSystem, FileSystemCapability, InMemoryFileSystem, RealDiskFileSystem,
    WriteBlocklistFileSystem,
};
#[cfg(feature = "hooks")]
#[cfg_attr(docsrs, doc(cfg(feature = "hooks")))]
pub use hooks::ShellHook;
pub use jsonl_log::JsonlEventLog;
#[cfg(feature = "mcp-oauth")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp-oauth")))]
pub use mcp::oauth::{
    McpOAuthLoginOptions, McpOAuthTokenProvider, McpOAuthTokenSet, PreparedMcpOAuthLogin,
    prepare_mcp_oauth_login, refresh_mcp_oauth_tokens,
};
#[cfg(feature = "mcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp")))]
pub use mcp::{
    DynamicMcpCapability, McpAuthProvider, McpCapability, McpClient, McpProtocolMode, McpServer,
    McpServerDiscovery, McpTransport, StaticBearer,
};

#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use drivers::{anthropic::AnthropicDriver, openai::OpenAiDriver};
