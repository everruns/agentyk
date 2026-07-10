//! Simulates a durable host driving a turn the way everruns' durable engine
//! would: each `TurnAction` is an "activity", the serialized `TurnState` and
//! the event log are the ONLY things that survive a crash, and history is
//! rebuilt by replaying the log. If this test passes, the execution
//! abstraction is sufficient for durable execution.

use std::sync::Arc;

use agentyk::{
    Agent, EventData, EventLog, EventRequest, FnTool, InMemoryEventLog, JsonlEventLog, ModelSpec,
    Result, SimDriver, SimTurn, ToolContext, ToolOutput, TurnAction, TurnOutcome, TurnState, atoms,
    messages_from_events,
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
    for data in effects {
        log.append(EventRequest::with_turn(
            state.session_id,
            state.turn_id,
            data,
        ))
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn durable_host_drives_turn_across_a_crash() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("events.jsonl");

    // The agent value — both "processes" compose it identically, the way a
    // durable worker rebuilds its environment on every activity.
    let build_agent = |script: Vec<SimTurn>| {
        Agent::builder()
            .system_prompt("You do arithmetic.")
            .model(ModelSpec::llmsim())
            .driver(SimDriver::new(script))
            .tool(add_tool())
            .build()
    };

    // ---- Process 1: start the turn, reason once, then "crash". ----
    let checkpoint: String;
    {
        let agent = build_agent(vec![SimTurn::tool_call("add", json!({"a": 20, "b": 22}))])?;
        // Durable hosts drive the machine directly instead of Session::run.
        let log = JsonlEventLog::new(&log_path)?;
        let session_id = agentyk::SessionId::new();

        let assembled =
            atoms::assemble(agent.system_prompt(), agent.capabilities(), session_id).await?;
        let driver = agent.driver_for_model().expect("driver");

        let input = agentyk::Message::user("what is 20 + 22?");
        let (mut state, effects) = TurnState::start(session_id, 4, &input);
        record(&log, &state, effects).await?;

        // Activity: reason.
        assert_eq!(state.next_action(), TurnAction::Reason);
        let started = state.on_reason_started(Some(&agent.model().model));
        record(&log, &state, started).await?;
        let history = messages_from_events(&log.read(session_id).await?);
        let response = atoms::reason(driver.as_ref(), agent.model(), &assembled, history).await?;
        let effects = state.on_reason_completed(&response);
        record(&log, &state, effects).await?;

        // Checkpoint the state; everything else in this scope is dropped —
        // the "crash".
        checkpoint = serde_json::to_string(&state)?;
    }

    // ---- Process 2: resume purely from checkpoint + log. ----
    let agent = build_agent(vec![SimTurn::text("The answer is 42.")])?;
    let log = JsonlEventLog::new(&log_path)?;
    let mut state: TurnState = serde_json::from_str(&checkpoint)?;
    assert!(!state.is_complete());

    let assembled =
        atoms::assemble("You do arithmetic.", agent.capabilities(), state.session_id).await?;
    let driver = agent.driver_for_model().expect("driver");

    loop {
        match state.next_action() {
            TurnAction::Reason => {
                let started = state.on_reason_started(Some(&agent.model().model));
                record(&log, &state, started).await?;
                let history = messages_from_events(&log.read(state.session_id).await?);
                let response =
                    atoms::reason(driver.as_ref(), agent.model(), &assembled, history).await?;
                let effects = state.on_reason_completed(&response);
                record(&log, &state, effects).await?;
            }
            TurnAction::ExecuteTool { call } => {
                let effects = state.on_tool_started(&call.id);
                record(&log, &state, effects).await?;
                let context = ToolContext {
                    session_id: state.session_id,
                    turn_id: state.turn_id,
                };
                let output = atoms::act(&assembled, &call, &context).await;
                let effects = state.on_tool_completed(&call.id, &output);
                record(&log, &state, effects).await?;
            }
            TurnAction::Complete(outcome) => {
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

    // The combined log is a complete, ordered turn: the tool ran with the
    // pre-crash arguments and produced 42.
    let events = log.read(state.session_id).await?;
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "turn.started",
            "input.message",
            "output.message.started",
            "output.message.completed",
            "tool.started",
            "tool.completed",
            "output.message.started",
            "output.message.completed",
            "turn.completed",
        ]
    );
    assert!(events.iter().any(|e| matches!(
        &e.data,
        EventData::ToolCompleted { output, .. } if output == "42"
    )));
    assert_eq!(state.iterations, 2);
    assert_eq!(state.tool_calls_executed, 1);
    Ok(())
}

/// The default executor and a manual machine drive must produce byte-for-byte
/// the same event sequence — the equivalence everruns relies on to keep
/// in-process and durable hosts behaviorally aligned.
#[tokio::test]
async fn manual_drive_matches_in_process_executor() -> Result<()> {
    let script = || {
        vec![
            SimTurn::tool_call("add", json!({"a": 1, "b": 2})),
            SimTurn::text("3"),
        ]
    };

    // In-process executor via Session::run.
    let agent = Agent::builder()
        .system_prompt("s")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new(script()))
        .tool(add_tool())
        .build()?;
    let executor_log = Arc::new(InMemoryEventLog::new());
    let mut session = agent.session_with_log(executor_log.clone());
    session.run("q").await?;
    let executor_types: Vec<String> = session
        .events()
        .await?
        .iter()
        .map(|e| e.event_type.clone())
        .collect();

    // Manual drive of the same machine.
    let agent = Agent::builder()
        .system_prompt("s")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new(script()))
        .tool(add_tool())
        .build()?;
    let log = InMemoryEventLog::new();
    let session_id = agentyk::SessionId::new();
    let assembled = atoms::assemble("s", agent.capabilities(), session_id).await?;
    let driver = agent.driver_for_model().expect("driver");

    let (mut state, effects) = TurnState::start(session_id, 16, &agentyk::Message::user("q"));
    record(&log, &state, effects).await?;
    loop {
        match state.next_action() {
            TurnAction::Reason => {
                let started = state.on_reason_started(Some(&agent.model().model));
                record(&log, &state, started).await?;
                let history = messages_from_events(&log.read(session_id).await?);
                let response =
                    atoms::reason(driver.as_ref(), agent.model(), &assembled, history).await?;
                let effects = state.on_reason_completed(&response);
                record(&log, &state, effects).await?;
            }
            TurnAction::ExecuteTool { call } => {
                let effects = state.on_tool_started(&call.id);
                record(&log, &state, effects).await?;
                let context = ToolContext {
                    session_id,
                    turn_id: state.turn_id,
                };
                let output = atoms::act(&assembled, &call, &context).await;
                let effects = state.on_tool_completed(&call.id, &output);
                record(&log, &state, effects).await?;
            }
            TurnAction::Complete(_) => break,
        }
    }
    let manual_types: Vec<String> = log
        .read(session_id)
        .await?
        .iter()
        .map(|e| e.event_type.clone())
        .collect();

    assert_eq!(executor_types, manual_types);
    Ok(())
}
