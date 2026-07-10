//! # agentyk-core
//!
//! The agentyk **contract crate**: everything a host or extension implements
//! against, and nothing else. If you are building an application, depend on
//! [`agentyk`](https://crates.io/crates/agentyk) (which re-exports all of
//! this); depend on `agentyk-core` directly when you are *implementing* a
//! seam — a custom [`tool::Tool`], [`capability::Capability`],
//! [`driver::ChatDriver`], [`event_log::EventLog`], or
//! [`executor::TurnExecutor`] (e.g. a durable execution host).
//!
//! Contents:
//!
//! - **Protocol data** — [`event`] (the event protocol), [`message`],
//!   [`tool`] definitions, typed [`id`]s, [`error`].
//! - **Seams** — the traits in [`capability`], [`tool`], [`driver`],
//!   [`event_log`], [`event`] (listeners), [`executor`], and [`hooks`]
//!   (pre/post tool interception).
//! - **The turn machine** — [`turn`] (pure, serializable) and the stateless
//!   [`atoms`] it sequences; [`replay`] folds an event log back into message
//!   history.
//!
//! This crate is deliberately lean: no tokio, no HTTP, no process spawning —
//! pure data, traits, and std-only implementations (like
//! [`event_log::InMemoryEventLog`]).

pub mod atoms;
pub mod cancellation;
pub mod capability;
pub mod driver;
pub mod error;
pub mod event;
pub mod event_log;
pub mod executor;
pub mod hooks;
pub mod id;
pub mod message;
pub mod replay;
pub mod tool;
pub mod turn;

pub use cancellation::CancellationToken;
pub use capability::{Capability, SystemPromptContext};
pub use driver::{
    ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, DriverRegistry, ModelSpec, Usage,
};
pub use error::{Error, LlmErrorKind, Result};
pub use event::{Event, EventData, EventListener, EventRequest, event_types};
pub use event_log::{EventLog, InMemoryEventLog};
pub use executor::{TurnExecutor, TurnHost, TurnResult};
pub use hooks::{PostToolExecHook, PreToolUseDecision, PreToolUseHook};
pub use id::{EventId, SessionId, TurnId};
pub use message::{ContentPart, ImageContentPart, Message, Role, TextContentPart, ToolCall};
pub use replay::messages_from_events;
pub use tool::{FnTool, Tool, ToolContext, ToolDefinition, ToolOutput};
pub use turn::{TurnAction, TurnOutcome, TurnPhase, TurnState};
