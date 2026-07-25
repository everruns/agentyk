//! The budget seam — deliberately stopping a turn that would otherwise keep
//! burning resources without producing useful work.
//!
//! Mirrors everruns' `HardLimitStopRule` + `BudgetChecker`: a host supplies
//! a [`BudgetChecker`] (token spend, dollar cost, wall-clock time —
//! agentyk doesn't need to know which); the engine asks it before each
//! turn action and, on [`BudgetDecision::Seal`], stops the turn via
//! [`crate::turn::TurnState::on_seal`] rather than letting it run to
//! [`crate::turn::TurnOutcome::MaxIterations`]. No default policy ships.

use async_trait::async_trait;

use crate::id::SessionId;

/// Whether a turn may take its next action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDecision {
    /// Nothing to stop for.
    Proceed,
    /// Stop the turn now, before the next action spends anything more.
    Seal,
}

/// Decides whether a turn may keep going.
///
/// A host supplies one to stop work that is no longer worth doing — token
/// spend, wall-clock, dollars. Core never defines what a budget is.
#[async_trait]
pub trait BudgetChecker: Send + Sync {
    /// Consulted once per turn action. Whatever "budget" means — tokens,
    /// money, wall-clock — is the implementation's business; core only asks
    /// whether to continue.
    ///
    /// Called on the turn's hot path, so keep it cheap: read a counter, do
    /// not query a billing API.
    async fn check(&self, session_id: SessionId) -> BudgetDecision;
}
