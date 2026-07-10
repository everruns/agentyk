//! # agentyk
//!
//! Compose agents from values and run them.
//!
//! An [`Agent`] is built entirely by value — a system prompt, a model
//! ([`ModelSpec`]), capabilities ([`Capability`]), tools ([`Tool`]) — with no
//! entity creation, no registration, and no ids to thread. Ids exist only as
//! internal correlation handles on sessions and events.
//!
//! The domain language (events protocol, capabilities, drivers, the
//! `input → reason → act` turn) is inherited from
//! [everruns](https://github.com/everruns/everruns); this crate is the
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
pub mod atoms;
pub mod capability;
pub mod driver;
pub mod drivers;
pub mod error;
pub mod event;
pub mod event_log;
pub mod executor;
pub mod id;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod message;
pub mod session;
pub mod tool;
pub mod turn;

pub use agent::{Agent, AgentBuilder};
pub use capability::{Capability, SystemPromptContext};
pub use driver::{
    ChatDriver, ChatRequest, ChatResponse, DriverId, DriverRegistry, ModelSpec, Usage,
};
pub use drivers::sim::{SimDriver, SimToolCall, SimTurn};
pub use error::{Error, Result};
pub use event::{Event, EventData, EventListener, EventRequest, event_types};
pub use event_log::{EventLog, InMemoryEventLog, JsonlEventLog};
pub use executor::{InProcessExecutor, TurnExecutor, TurnHost};
pub use id::{EventId, SessionId, TurnId};
#[cfg(feature = "mcp")]
pub use mcp::{McpCapability, McpClient, McpServer};
pub use message::{Message, Role, ToolCall};
pub use session::{Session, TurnResult, messages_from_events};
pub use tool::{FnTool, Tool, ToolContext, ToolDefinition, ToolOutput};
pub use turn::{TurnAction, TurnOutcome, TurnPhase, TurnState};

#[cfg(feature = "http")]
pub use drivers::{anthropic::AnthropicDriver, openai::OpenAiDriver};
