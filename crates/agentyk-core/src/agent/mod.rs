//! Agent — what an agent is composed of.
//!
//! The composition value ([`config::AgentConfig`]) and the pluggable pieces
//! that go into it: capabilities, tools, middleware, context assembly,
//! per-turn controls, budget policy, and the typed extension bag. Grouped as
//! a directory only: each module is re-exported at the crate root, so the
//! public paths are `agentyk_core::capability`, `agentyk_core::tool`, and so
//! on, unchanged.

pub mod budget;
pub mod capability;
pub mod config;
pub mod context;
pub mod controls;
pub mod extensions;
pub mod middleware;
pub mod tool;
