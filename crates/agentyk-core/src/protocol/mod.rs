//! Protocol — the shapes that cross a boundary.
//!
//! Everything here is serialized to an event log, sent to a provider, or
//! both, so these are the types whose *shape* is a compatibility promise.
//! Grouped as a directory only: each module is re-exported at the crate root,
//! so the public paths are `agentyk_core::event`, `agentyk_core::message`,
//! and so on, unchanged.

pub mod driver;
pub mod error;
pub mod event;
pub mod event_log;
pub mod id;
pub mod message;
