//! Budget sealing: a `BudgetChecker` can deliberately stop a turn.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use agentyk::{
    Agent, BudgetChecker, BudgetDecision, Error, ModelSpec, Provider, Result, SealReason,
    SessionId, SimDriver, SimTurn, event_types,
};

/// Seals as soon as it has been checked `seal_after` times.
struct CountingBudget {
    checks: AtomicUsize,
    seal_after: usize,
}

#[async_trait]
impl BudgetChecker for CountingBudget {
    async fn check(&self, _session_id: SessionId) -> BudgetDecision {
        let count = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= self.seal_after {
            BudgetDecision::Seal
        } else {
            BudgetDecision::Proceed
        }
    }
}

#[tokio::test]
async fn budget_exhaustion_seals_before_any_reason_call() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "should never run",
        )])))
        .budget_checker(CountingBudget {
            checks: AtomicUsize::new(0),
            seal_after: 1,
        })
        .build()?;

    let mut session = agent.session();
    let error = session
        .run("go")
        .await
        .expect_err("budget exhausted before the first reason call");
    assert!(matches!(error, Error::Sealed(SealReason::BudgetExhausted)));

    let events = session.events().await?;
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            event_types::TURN_STARTED,
            event_types::INPUT_MESSAGE,
            event_types::TURN_SEALED,
        ]
    );
    Ok(())
}

#[tokio::test]
async fn budget_allows_a_few_iterations_then_seals() -> Result<()> {
    use serde_json::json;

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("noop", json!({})),
            SimTurn::tool_call("noop", json!({})),
            SimTurn::text("should never be reached"),
        ])))
        .max_iterations(10)
        // Checked once at turn start, once before each of the two tool
        // calls, once before the third reason: seal on the 4th check.
        .budget_checker(CountingBudget {
            checks: AtomicUsize::new(0),
            seal_after: 4,
        })
        .build()?;

    let mut session = agent.session();
    let error = session
        .run("go")
        .await
        .expect_err("budget exhausted mid-turn");
    assert!(matches!(error, Error::Sealed(SealReason::BudgetExhausted)));

    // The turn made real progress (a tool actually ran) before sealing —
    // sealing abandons what's pending, it doesn't undo what already ran.
    let events = session.events().await?;
    assert!(
        events
            .iter()
            .any(|e| e.event_type == event_types::TOOL_COMPLETED)
    );
    assert_eq!(events.last().unwrap().event_type, event_types::TURN_SEALED);
    Ok(())
}

#[tokio::test]
async fn no_budget_checker_never_seals() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("done")])))
        .build()?;

    let turn = agent.run("go").await?;
    assert_eq!(turn.response, "done");
    Ok(())
}
