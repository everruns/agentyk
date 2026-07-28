//! Simulates a durable host driving a turn the way everruns' durable engine
//! would: each `TurnAction` is an "activity", the serialized `TurnState` and
//! the event log are the ONLY things that survive a crash, and history is
//! rebuilt by replaying the log. If this test passes, the execution
//! abstraction is sufficient for durable execution.

use std::sync::{Arc, Mutex};

use agentyk::{
    Agent, EventData, EventLog, EventRequest, ExpectedVersion, FnTool, History, Hook, HookEvent,
    HookOutcome, HookPayload, InMemoryEventLog, ModelSpec, Result, SimDriver, SimTurn, ToolContext,
    ToolOutput, TurnEngine, TurnHost, TurnOperation, TurnOutcome, TurnState, atoms,
};
use async_trait::async_trait;
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

struct RecordingHook {
    event: HookEvent,
    seen: Arc<Mutex<Vec<HookEvent>>>,
}

#[async_trait]
impl Hook for RecordingHook {
    fn id(&self) -> &str {
        "durable-test"
    }

    fn event(&self) -> HookEvent {
        self.event
    }

    async fn run(&self, payload: &HookPayload) -> HookOutcome {
        self.seen.lock().unwrap().push(payload.event);
        if payload.event == HookEvent::UserPromptSubmit {
            HookOutcome::Mutate {
                patch: json!({"message": "rewritten by hook"}),
                reason: None,
            }
        } else {
            HookOutcome::Allow
        }
    }
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
    let seen_hooks = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .system_prompt("You do arithmetic.")
        .model(ModelSpec::llmsim())
        .max_iterations(4)
        .driver(SimDriver::new([
            SimTurn::tool_call("add", json!({"a": 20, "b": 22})),
            SimTurn::text("The answer is 42."),
        ]))
        .tool(add_tool())
        .hook(RecordingHook {
            event: HookEvent::UserPromptSubmit,
            seen: seen_hooks.clone(),
        })
        .hook(RecordingHook {
            event: HookEvent::TurnEnd,
            seen: seen_hooks.clone(),
        })
        .build()?;
    let log = InMemoryEventLog::new();
    let session_id = agentyk::SessionId::new();
    let input = agentyk::Message::user("what is 20 + 22?");
    let engine = TurnEngine;
    let mut bootstrap_history = History::new();
    let bootstrap_host = TurnHost::new(
        session_id,
        agent.definition(),
        agent.environment(),
        &log,
        &mut bootstrap_history,
    );
    let started = engine.start(&bootstrap_host, input).await;
    assert!(started.rejection.is_none());
    let turn_id = started.state.turn_id;
    record(&log, &started.state, started.events).await?;
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
                let effects = engine
                    .finish(
                        &host,
                        turn_id,
                        json!({"success": true, "response": "The answer is 42."}),
                    )
                    .await;
                record(&log, &state, effects).await?;
                break;
            }
        }
    }

    let state = TurnState::replay(&log.read(session_id).await?, turn_id)?;
    assert_eq!(state.iterations, 2);
    assert_eq!(state.tool_calls_executed, 1);
    assert!(state.is_complete());
    assert_eq!(
        *seen_hooks.lock().unwrap(),
        [HookEvent::UserPromptSubmit, HookEvent::TurnEnd]
    );
    assert_eq!(
        state.phase,
        agentyk::TurnPhase::Completed(TurnOutcome::Success {
            response: "The answer is 42.".into(),
        })
    );
    assert!(log.read(session_id).await?.iter().any(|event| {
        matches!(
            &event.data,
            EventData::InputMessage { message } if message.text() == "rewritten by hook"
        )
    }));
    Ok(())
}
