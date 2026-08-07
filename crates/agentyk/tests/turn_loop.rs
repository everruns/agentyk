//! End-to-end tests of the turn loop over the scripted sim driver.

use std::sync::Arc;

use agentyk::{
    Agent, Capability, EventData, FnTool, InMemoryEventLog, ModelSpec, Provider, Result, SimDriver,
    SimTurn, SystemPromptContext, ToolOutput, event_types, messages_from_events,
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

#[tokio::test]
async fn turn_with_tool_call() -> Result<()> {
    let sim = SimDriver::new([
        SimTurn::tool_call("add", json!({"a": 2, "b": 3})),
        SimTurn::text("The sum is 5."),
    ]);

    let agent = Agent::builder()
        .name("calculator")
        .system_prompt("You do arithmetic with tools.")
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(sim))
        .tool(add_tool())
        .build()?;

    let mut session = agent.session();
    let turn = session.run("what is 2 + 3?").await?;

    assert_eq!(turn.response, "The sum is 5.");
    assert_eq!(turn.iterations, 2);
    assert_eq!(turn.tool_calls, 1);

    // Event protocol: full lifecycle recorded, in order. Streaming deltas
    // are ephemeral and never reach the log — see the durable_simulation
    // and streaming tests for those.
    let events = session.events().await?;
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            event_types::TURN_STARTED,
            event_types::INPUT_MESSAGE,
            event_types::OUTPUT_MESSAGE_STARTED,
            event_types::OUTPUT_MESSAGE_COMPLETED, // assistant tool-call message
            event_types::TOOL_STARTED,
            event_types::TOOL_COMPLETED,
            event_types::OUTPUT_MESSAGE_STARTED,
            event_types::OUTPUT_MESSAGE_COMPLETED, // final answer
            event_types::TURN_COMPLETED,
        ]
    );
    // Sequences are contiguous from 1 — every persisted event is durable.
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, Some(index as u64 + 1));
    }
    // The tool actually ran.
    assert!(events.iter().any(|e| matches!(
        &e.data,
        EventData::ToolCompleted { output, is_error, .. } if output == "5" && !is_error
    )));
    Ok(())
}

#[tokio::test]
async fn unknown_tool_becomes_error_result_and_model_recovers() -> Result<()> {
    let sim = SimDriver::new([
        SimTurn::tool_call("does_not_exist", json!({})),
        SimTurn::text("recovered"),
    ]);
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(sim))
        .build()?;

    let turn = agent.run("go").await?;
    assert_eq!(turn.response, "recovered");
    assert_eq!(turn.tool_calls, 1);
    Ok(())
}

#[tokio::test]
async fn max_iterations_fails_the_turn() -> Result<()> {
    // A script that never stops calling tools.
    let sim = SimDriver::new([
        SimTurn::tool_call("add", json!({"a": 1, "b": 1})),
        SimTurn::tool_call("add", json!({"a": 1, "b": 1})),
        SimTurn::tool_call("add", json!({"a": 1, "b": 1})),
    ]);
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(sim))
        .tool(add_tool())
        .max_iterations(2)
        .build()?;

    let mut session = agent.session();
    let error = session.run("loop forever").await.unwrap_err();
    assert!(error.to_string().contains("max iterations"));

    let events = session.events().await?;
    assert_eq!(events.last().unwrap().event_type, event_types::TURN_FAILED);
    Ok(())
}

struct PirateCapability;

#[async_trait]
impl Capability for PirateCapability {
    fn id(&self) -> &str {
        "pirate"
    }

    async fn system_prompt_contribution(&self, _context: &SystemPromptContext) -> Option<String> {
        Some("Always answer like a pirate.".to_string())
    }
}

#[tokio::test]
async fn capability_contributes_to_system_prompt() -> Result<()> {
    let sim = Arc::new(SimDriver::new([SimTurn::text("arr")]));
    let agent = Agent::builder()
        .system_prompt("Base prompt.")
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(ForwardingDriver(sim.clone())))
        .capability(PirateCapability)
        .build()?;

    agent.run("hi").await?;

    let requests = sim.recorded_requests();
    let system = requests[0].system_prompt.clone().unwrap();
    assert!(system.starts_with("Base prompt."));
    assert!(system.contains("Always answer like a pirate."));
    Ok(())
}

/// Wraps an `Arc<SimDriver>` so a test can keep a handle for assertions.
struct ForwardingDriver(Arc<SimDriver>);

#[async_trait]
impl agentyk::ChatDriver for ForwardingDriver {
    async fn complete(&self, request: agentyk::ChatRequest) -> Result<agentyk::ChatResponse> {
        self.0.complete(request).await
    }
}

#[tokio::test]
async fn resume_rebuilds_history_from_the_log() -> Result<()> {
    let log = Arc::new(InMemoryEventLog::new());

    let agent_one = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "first answer",
        )])))
        .build()?;
    let mut session = agent_one.session_with_log(log.clone());
    session.run("first question").await?;
    let session_id = session.id();
    let live_messages = session.messages().to_vec();
    drop(session);

    // A fresh agent instance resumes purely from the log.
    let agent_two = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "second answer",
        )])))
        .build()?;
    let mut resumed = agent_two.resume_session(log, session_id).await?;
    assert_eq!(resumed.messages(), live_messages.as_slice());

    let turn = resumed.run("second question").await?;
    assert_eq!(turn.response, "second answer");
    assert_eq!(resumed.messages().len(), 4);
    Ok(())
}

/// The live history a running turn sends to the model must be exactly what a
/// replay of the log produces — otherwise a resumed session would silently
/// see a different conversation than the one that ran. `History` exists to
/// make the two impossible to diverge; this pins the property end to end,
/// across a turn with tool calls and streaming.
#[tokio::test]
async fn live_history_always_equals_a_replay_of_the_log() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("add", json!({"a": 2, "b": 3})),
            SimTurn::tool_call("add", json!({"a": 10, "b": 1})),
            SimTurn::text("11"),
        ])))
        .tool(add_tool())
        .build()?;

    let mut session = agent.session();
    session.run("add things").await?;
    session.run("again").await?;

    let replayed = messages_from_events(&session.events().await?);
    assert_eq!(session.messages(), replayed.as_slice());
    // Not vacuous: the turn really did accumulate input, output and tool
    // results.
    assert!(replayed.len() > 4, "{replayed:?}");
    Ok(())
}

#[tokio::test]
async fn missing_model_is_a_build_error() {
    let error = Agent::builder().build().unwrap_err();
    assert!(error.to_string().contains("model is required"));
}

#[tokio::test]
async fn an_unregistered_provider_is_a_build_error_listing_the_registered_ones() {
    let error = Agent::builder()
        .model(ModelSpec::on("no-such-service", "m"))
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .build()
        .unwrap_err();

    // The usual cause is a typo in config, so the message has to say what
    // *is* registered rather than only what is missing.
    let message = error.to_string();
    assert!(message.contains("no-such-service"), "{message}");
    assert!(message.contains("llmsim"), "{message}");
}
