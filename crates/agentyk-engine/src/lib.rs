//! The canonical Agentyk turn engine.
//!
//! This crate owns [`Agent`], [`Session`], and the one interpretation of turn
//! semantics shared by immediate and durable execution hosts. Applications
//! normally depend on the `agentyk` facade, which re-exports this crate.

pub mod agent;
pub mod engine;
pub mod host;
pub mod in_process;
pub mod session;

mod config;
mod hooks;

pub use agent::{Agent, AgentBuilder, AgentDefinition, AgentEnvironment};
pub use engine::{
    HookRejection, PreparedStep, PreparedToolCall, PreparedTurnStart, TurnEngine, TurnOperation,
};
pub use host::{TurnHost, TurnResult};
pub use in_process::InProcessExecutor;
pub use session::{RunOptions, Session, SessionView};
