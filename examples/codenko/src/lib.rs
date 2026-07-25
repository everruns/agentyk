//! codenko — a small terminal coding agent.
//!
//! One agent, four tools, one screen. The point of the example is the seams it
//! uses, all of which are agentyk's public contract:
//!
//! - **Composition** — [`agent::build_agent`] builds the whole agent by value:
//!   prompt, model, a filesystem capability, a shell tool, an approval hook.
//! - **The event log as the UI's source of truth** — [`transcript::Transcript`]
//!   is a fold over the agentyk event stream, so the display is a pure
//!   function of protocol events and can be tested without a terminal.
//! - **Streaming** — ephemeral `output.message.delta` events grow the
//!   assistant's reply in place as it arrives.
//! - **Interception** — a [`agentyk::PreToolUseHook`] turns mutating tool calls
//!   into an approval prompt and blocks the turn until the operator answers.
//! - **Cancellation** — a [`agentyk::CancellationToken`] per turn, so esc stops
//!   a long run without tearing down the session.
//!
//! The binary wires these to a [`tuika`] full-screen UI; see [`ui::App`].

pub mod agent;
pub mod config;
pub mod transcript;
pub mod ui;
