//! # agentyk
//!
//! Compose agents from values and run them.
//!
//! An [`Agent`] is built entirely by value — a system prompt, a model
//! ([`ModelSpec`]), capabilities ([`Capability`]), tools ([`Tool`]) — with no
//! entity creation, no registration, and no ids to thread. Ids exist only as
//! internal correlation handles on sessions and events.
//!
//! This is the **framework crate**: builders, the default in-process
//! executor, bundled drivers (sim; OpenAI/Anthropic behind `http`), the JSONL
//! event log, and MCP support. The contract it drives — traits, the event
//! protocol, and the turn state machine — lives in
//! [`agentyk-core`](https://crates.io/crates/agentyk-core) and is fully
//! re-exported here; depend on core directly only when *implementing* a seam
//! (a custom driver, capability, event log, or executor).
//!
//! The domain language (events protocol, capabilities, drivers, the
//! `input → reason → act` turn) is inherited from
//! [everruns](https://github.com/everruns/everruns); agentyk is the
//! value-first core that everruns-core and everruns-runtime are intended to
//! be rebuilt on top of.
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
//! ```ignore
//! let agent = Agent::builder()
//!     .system_prompt("You are a coding agent.")
//!     .model(ModelSpec::anthropic("claude-sonnet-4-5").api_key(key))
//!     .capability(my_capability)
//!     .build()?;
//! let result = agent.run("list the files").await?;
//! ```

pub mod agent;
pub mod drivers;
pub mod in_process;
pub mod jsonl_log;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod session;

// The full contract: protocol data, seams, the turn machine, atoms, replay.
pub use agentyk_core::*;

pub use agent::{Agent, AgentBuilder};
pub use drivers::sim::{SimDriver, SimToolCall, SimTurn};
pub use in_process::InProcessExecutor;
pub use jsonl_log::JsonlEventLog;
#[cfg(feature = "mcp")]
pub use mcp::{McpCapability, McpClient, McpServer};
pub use session::Session;

#[cfg(feature = "http")]
pub use drivers::{anthropic::AnthropicDriver, openai::OpenAiDriver};
