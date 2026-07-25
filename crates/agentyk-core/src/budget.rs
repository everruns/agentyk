//! The budget seam — deliberately stopping a turn that would otherwise keep
//! burning resources without producing useful work.
//!
//! Mirrors everruns' `HardLimitStopRule` + `BudgetChecker`: a host supplies
//! a [`BudgetChecker`] (token spend, dollar cost, wall-clock time —
//! agentyk doesn't need to know which); the executor asks it before each
//! turn action and, on [`BudgetDecision::Seal`], stops the turn via
//! [`crate::turn::TurnState::on_seal`] rather than letting it run to
//! [`crate::turn::TurnOutcome::MaxIterations`]. No default policy ships —
//! [`crate::config::AgentConfig::budget_checker`] is `None` unless a host
//! sets one.

use async_trait::async_trait;

use crate::id::SessionId;

/// Whether a turn may take its next action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDecision {
    Proceed,
    Seal,
}

#[async_trait]
pub trait BudgetChecker: Send + Sync {
    async fn check(&self, session_id: SessionId) -> BudgetDecision;
}
