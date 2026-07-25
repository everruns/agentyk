//! Simulates a durable host driving a turn the way everruns' durable engine
//! would: each `TurnAction` is an "activity", the serialized `TurnState` and
//! the event log are the ONLY things that survive a crash, and history is
//! rebuilt by replaying the log. If this test passes, the execution
//! abstraction is sufficient for durable execution.

use agentyk::{
    Agent, EventData, EventLog, EventRequest, ExpectedVersion, FnTool, History, InMemoryEventLog,
    ModelSpec, Result, SimDriver, SimTurn, ToolContext, ToolOutput, TurnEngine, TurnHost,
    TurnOperation, TurnOutcome, TurnState, atoms,
};
use serde_json::json;

fn add_tool() -> FnTool {
    FnTool::new(
        "add",
        "Add two numbers.",
        json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]
        }),
        |args| async move {
            let a = args["a"].as_f64().unwrap_or(0.0);
            let b = args["b"].as_f64().unwrap_or(0.0);
            ToolOutput::text((a + b).to_string())
        },
    )
}

/// A durable host's write path: persist effects, exactly like the executor
/// does, but standalone.
async fn record(
    log: &dyn agentyk::EventLog,
    state: &TurnState,
    effects: Vec<EventData>,
) -> Result<()> {
    let version = log
        .read(state.session_id)
        .await?
        .last()
        .and_then(|event| event.sequence)
        .unwrap_or(0);
    let requests = effects
        .into_iter()
        .map(|data| EventRequest::with_turn(state.session_id, state.turn_id, data))
        .collect();
    log.append_batch(state.session_id, ExpectedVersion::Exact(version), requests)
        .await?;
    Ok(())
}

#[tokio::test]
async fn durable_host_replays_state_between_every_engine_step() -> Result<()> {
    let agent = Agent::builder()
        .system_prompt("You do arithmetic.")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("add", json!({"a": 20, "b": 22})),
            SimTurn::text("The answer is 42."),
        ]))
        .tool(add_tool())
        .build()?;
    let log = InMemoryEventLog::new();
    let session_id = agentyk::SessionId::new();
    let input = agentyk::Message::user("what is 20 + 22?");
    let (initial, effects) = TurnState::start(session_id, 4, &input);
    let turn_id = initial.turn_id;
    record(&log, &initial, effects).await?;

    let engine = TurnEngine;
    let mut bootstrap_history = History::new();
    let bootstrap_host = TurnHost::new(
        session_id,
        agent.definition(),
        agent.environment(),
        &log,
        &mut bootstrap_history,
    );
    let assembled = engine.assemble(&bootstrap_host).await?;
    let driver = agent.driver_for_model().expect("driver");

    loop {
        // No serialized TurnState survives this boundary. Every activity
        // starts by reducing the durable stream.
        let events = log.read(session_id).await?;
        let mut state = TurnState::replay(&events, turn_id)?;
        let mut history = History::from_events(&events);
        let host = TurnHost::new(
            session_id,
            agent.definition(),
            agent.environment(),
            &log,
            &mut history,
        );
        let step = engine.prepare(&host, &assembled, &mut state).await?;
        record(&log, &state, step.events).await?;

        // Rehydrate again after the prepare transition, before executing the
        // scheduled operation.
        let events = log.read(session_id).await?;
        let mut state = TurnState::replay(&events, turn_id)?;
        match step.operation {
            TurnOperation::InvokeModel { request } => {
                let response = driver.complete(request).await?;
                let effects = engine.complete_model(&mut state, &response);
                record(&log, &state, effects).await?;
            }
            TurnOperation::InvokeTools { calls } => {
                for prepared in calls {
                    let was_denied = prepared.denied.is_some();
                    let output = match prepared.denied {
                        Some(output) => output,
                        None => {
                            let context = ToolContext::new(session_id, turn_id);
                            atoms::act(&assembled, &prepared.call, &context).await
                        }
                    };
                    let mut history = History::from_events(&events);
                    let host = TurnHost::new(
                        session_id,
                        agent.definition(),
                        agent.environment(),
                        &log,
                        &mut history,
                    );
                    let effects = engine
                        .complete_tool(
                            &host,
                            &assembled,
                            &mut state,
                            &prepared.call,
                            output,
                            was_denied,
                        )
                        .await;
                    record(&log, &state, effects).await?;
                }
            }
            TurnOperation::Finished(outcome) => {
                assert_eq!(
                    outcome,
                    TurnOutcome::Success {
                        response: "The answer is 42.".into()
                    }
                );
                break;
            }
        }
    }

    let state = TurnState::replay(&log.read(session_id).await?, turn_id)?;
    assert_eq!(state.iterations, 2);
    assert_eq!(state.tool_calls_executed, 1);
    assert!(state.is_complete());
    Ok(())
}
