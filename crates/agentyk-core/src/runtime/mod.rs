//! Runtime — how a turn actually runs.
//!
//! The sans-IO state machine, the stateless atoms it sequences, the executor
//! seam that drives them, the replay fold, and cancellation. Grouped as a
//! directory only: each module is re-exported at the crate root, so the
//! public paths are `agentyk_core::turn`, `agentyk_core::atoms`, and so on,
//! unchanged.

pub mod atoms;
pub mod cancellation;
pub mod executor;
pub mod replay;
pub mod turn;
