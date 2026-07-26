//! How a prepared tool batch is dispatched: concurrently by default, in
//! recorded order either way.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentyk::{
    Agent, EventData, ModelSpec, Result, SimDriver, SimToolCall, SimTurn, Tool, ToolContext,
    ToolDefinition, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;

/// A tool that records when it starts and finishes, and sleeps in between.
/// Two of these overlapping is the observable difference between concurrent
/// and sequential dispatch.
struct SlowTool {
    name: &'static str,
    delay: Duration,
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for SlowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.name, "Takes a while.", json!({"type": "object"}))
    }

    async fn execute(&self, _arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:start", self.name));
        tokio::time::sleep(self.delay).await;
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:end", self.name));
        ToolOutput::text(self.name)
    }
}

fn batching_driver() -> SimDriver {
    SimDriver::new([
        SimTurn::tool_calls([
            SimToolCall::new("slow", json!({})),
            SimToolCall::new("quick", json!({})),
        ]),
        SimTurn::text("both done"),
    ])
}

#[tokio::test]
async fn a_batch_runs_concurrently_by_default() -> Result<()> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(batching_driver())
        .tool(SlowTool {
            name: "slow",
            delay: Duration::from_millis(80),
            log: log.clone(),
        })
        .tool(SlowTool {
            name: "quick",
            delay: Duration::from_millis(1),
            log: log.clone(),
        })
        .build()?;

    let mut session = agent.session();
    session.run("do both").await?;

    // Both started before either finished, and the quick one finished first —
    // neither is possible under sequential dispatch.
    let seen = log.lock().expect("log").clone();
    assert_eq!(
        seen,
        vec!["slow:start", "quick:start", "quick:end", "slow:end"],
        "the batch did not overlap"
    );
    Ok(())
}

#[tokio::test]
async fn results_are_recorded_in_batch_order_not_completion_order() -> Result<()> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(batching_driver())
        .tool(SlowTool {
            name: "slow",
            delay: Duration::from_millis(80),
            log: log.clone(),
        })
        .tool(SlowTool {
            name: "quick",
            delay: Duration::from_millis(1),
            log,
        })
        .build()?;

    let mut session = agent.session();
    session.run("do both").await?;

    // `quick` finished first, but the log — and therefore any replay of this
    // session — still reads in the order the model asked for.
    let completed: Vec<String> = session
        .events()
        .await?
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ToolCompleted { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(completed, vec!["slow", "quick"]);
    Ok(())
}

#[tokio::test]
async fn a_denied_call_in_a_batch_does_not_block_the_others() -> Result<()> {
    use agentyk::{ToolCallDecision, ToolInvocation, TurnMiddleware};

    struct DenyQuick;

    #[async_trait]
    impl TurnMiddleware for DenyQuick {
        fn name(&self) -> &str {
            "deny-quick"
        }

        async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
            match invocation.call.name.as_str() {
                "quick" => ToolCallDecision::Deny {
                    reason: "not this one".into(),
                },
                _ => ToolCallDecision::Proceed,
            }
        }
    }

    let log = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(batching_driver())
        .tool(SlowTool {
            name: "slow",
            delay: Duration::from_millis(1),
            log: log.clone(),
        })
        .tool(SlowTool {
            name: "quick",
            delay: Duration::from_millis(1),
            log: log.clone(),
        })
        .middleware(DenyQuick)
        .build()?;

    let mut session = agent.session();
    session.run("do both").await?;

    // The denial resolves without running, and its error result still lands in
    // the right slot.
    assert_eq!(log.lock().expect("log").len(), 2, "only `slow` ran");
    let results: Vec<(String, bool)> = session
        .events()
        .await?
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ToolCompleted { name, is_error, .. } => Some((name, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![("slow".to_string(), false), ("quick".to_string(), true)]
    );
    Ok(())
}
