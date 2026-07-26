//! # agentyk-core
//!
//! The agentyk **contract crate**: everything a host or extension implements
//! against, and nothing else. If you are building an application, depend on
//! [`agentyk`](https://crates.io/crates/agentyk) (which re-exports all of
//! this); depend on `agentyk-core` directly when you are *implementing* a
//! seam — a custom [`tool::Tool`], [`capability::Capability`],
//! [`driver::ChatDriver`] or [`event_log::EventLog`].
//!
//! Contents:
//!
//! - **Protocol data** — [`event`] (the event protocol), [`message`],
//!   [`tool`] definitions, typed [`id`]s, [`error`].
//! - **Seams** — the traits in [`capability`], [`tool`], [`driver`],
//!   [`event_log`], [`event`] (listeners), and [`middleware`]
//!   (interception inside the turn loop).
//! - **The turn machine** — [`turn`] (pure, serializable) and the stateless
//!   [`atoms`] it sequences; [`replay`] folds an event log back into message
//!   history.
//!
//! This crate is deliberately lean: no tokio, no HTTP, no process spawning —
//! pure data, traits, and std-only implementations (like
//! [`event_log::InMemoryEventLog`]).
//!
//! ## `#[non_exhaustive]`, and where it is deliberately absent
//!
//! Core is the surface third parties implement against, so growing a type
//! must not be a breaking change — *unless* silence would be a bug. The rule:
//!
//! - **Data types are `#[non_exhaustive]`** — [`event::EventData`],
//!   [`error::Error`], [`driver::ModelSpec`], [`driver::ChatRequest`],
//!   [`driver::Usage`], [`tool::ToolDefinition`],
//!   and friends. They gain fields and variants as providers and hosts grow;
//!   each has a constructor plus setters, so adding one costs downstream
//!   nothing.
//! - **Contract types are exhaustive on purpose** — [`turn::TurnAction`],
//!   [`turn::TurnOutcome`], [`message::Role`], [`message::ContentPart`],
//!   [`middleware::ToolCallDecision`]. A host that does not handle a new turn
//!   action, or a driver that does not translate a new content part, is
//!   *wrong*; the compile error is the feature. Adding a variant here is a
//!   deliberate breaking change and belongs in a release note.

// Physical files and public modules deliberately share the same domain names.
// The source tree is the first architecture map maintainers encounter.
pub mod atoms;
pub mod budget;
pub mod cancellation;
pub mod capability;
pub mod concurrency;
pub mod context;
pub mod controls;
pub mod driver;
pub mod error;
pub mod event;
pub mod event_log;
pub mod extensions;
pub mod id;
pub mod input;
pub mod message;
pub mod middleware;
pub mod profile;
pub mod replay;
pub mod tool;
pub mod turn;

pub use budget::{BudgetChecker, BudgetDecision};
pub use cancellation::CancellationToken;
pub use capability::{Capability, CommandContext, CommandDescriptor, SystemPromptContext};
pub use context::{ContextAssembler, PassthroughContextAssembler};
pub use controls::TurnControls;
pub use driver::{
    ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, DriverRegistry, ModelSpec,
    ReasoningConfig, Usage,
};
pub use error::{Error, LlmErrorKind, Result};
pub use event::{Event, EventData, EventListener, EventRequest, event_types};
pub use event_log::{EventLog, EventStore, ExpectedVersion, InMemoryEventLog};
pub use extensions::Extensions;
pub use id::{EventId, MessageId, SessionId, TurnId};
pub use input::InputQueue;
pub use message::{ContentPart, ImageContentPart, Message, Role, TextContentPart, ToolCall};
pub use middleware::{
    ToolCallDecision, ToolChainOutcome, ToolInvocation, TurnMiddleware, after_tool_chain,
    before_tool_chain,
};
pub use profile::{InMemoryModelCatalog, ModelCatalog, ModelProfile};
pub use replay::{History, messages_from_events};
pub use tool::{
    DeferrablePolicy, FnTool, Tool, ToolContext, ToolDefinition, ToolOutput, ToolPolicy,
    ToolProgress, ToolProgressSink,
};
pub use turn::{SealReason, TurnAction, TurnOutcome, TurnPhase, TurnState};
