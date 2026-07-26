//! Steering: input that arrives while a turn is already running.

use std::sync::Arc;

use agentyk::{
    Agent, ChatDriver, ChatRequest, ChatResponse, DriverId, EventData, InputQueue, Message,
    ModelSpec, Result, SimDriver, SimTurn, Usage, event_types,
};
use async_trait::async_trait;
use serde_json::json;

/// A driver that steers the turn from inside its own first completion — the
/// deterministic stand-in for a user typing while the agent works.
struct SteeringDriver {
    /// Set to the session's queue once the session exists — a real steering
    /// source (a UI task, a socket) gets the handle the same way.
    input: std::sync::Mutex<Option<InputQueue>>,
    calls: std::sync::Mutex<Vec<ChatRequest>>,
}

impl SteeringDriver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            input: std::sync::Mutex::new(None),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn steer_through(&self, queue: InputQueue) {
        *self.input.lock().expect("input") = Some(queue);
    }
}

#[async_trait]
impl ChatDriver for SteeringDriver {
    fn id(&self) -> DriverId {
        DriverId::llmsim()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let call = {
            let mut calls = self.calls.lock().expect("calls");
            calls.push(request);
            calls.len()
        };
        match call {
            // First step: ask for a tool, and meanwhile the operator types.
            1 => {
                self.input
                    .lock()
                    .expect("input")
                    .as_ref()
                    .expect("steering queue attached")
                    .push(Message::user("actually, stop after this"));
                Ok(ChatResponse::new(
                    Message::assistant_with_calls(
                        "",
                        vec![agentyk::ToolCall {
                            id: "call_1".into(),
                            name: "noop".into(),
                            arguments: json!({}),
                        }],
                    ),
                    Usage::default(),
                ))
            }
            _ => Ok(ChatResponse::new(
                Message::assistant("stopping"),
                Usage::default(),
            )),
        }
    }
}

fn noop_tool() -> agentyk::FnTool {
    agentyk::FnTool::new(
        "noop",
        "Does nothing.",
        json!({"type": "object"}),
        |_| async { agentyk::ToolOutput::text("ok") },
    )
}

#[tokio::test]
async fn a_message_pushed_mid_turn_reaches_the_next_model_call() -> Result<()> {
    let driver = SteeringDriver::new();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SteeringHandle(driver.clone()))
        .tool(noop_tool())
        .build()?;

    let mut session = agent.session();
    driver.steer_through(session.input());
    let turn = session.run("do the thing").await?;
    assert_eq!(turn.response, "stopping");

    let calls = driver.calls.lock().expect("calls");
    assert_eq!(calls.len(), 2, "one tool round-trip");
    // The first request predates the steering; the second carries it, after
    // the tool result, in the order it was said.
    assert!(
        !calls[0]
            .messages
            .iter()
            .any(|m| m.text().contains("stop after this"))
    );
    let second: Vec<String> = calls[1].messages.iter().map(|m| m.text()).collect();
    assert!(
        second.iter().any(|text| text.contains("stop after this")),
        "steering must reach the model: {second:?}"
    );
    assert_eq!(
        second.last().map(String::as_str),
        Some("actually, stop after this"),
        "and arrive after the tool result, not before it: {second:?}"
    );
    Ok(())
}

/// Newtype so the test can keep an `Arc` to the driver and still hand the
/// builder an owned value.
struct SteeringHandle(Arc<SteeringDriver>);

#[async_trait]
impl ChatDriver for SteeringHandle {
    fn id(&self) -> DriverId {
        self.0.id()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.0.complete(request).await
    }
}

#[tokio::test]
async fn steering_is_recorded_so_a_replay_sees_the_same_conversation() -> Result<()> {
    let driver = SteeringDriver::new();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SteeringHandle(driver.clone()))
        .tool(noop_tool())
        .build()?;

    let mut session = agent.session();
    driver.steer_through(session.input());
    session.run("do the thing").await?;

    let events = session.events().await?;
    let inputs: Vec<String> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::InputMessage { message } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(inputs, vec!["do the thing", "actually, stop after this"]);

    // History is the fold of the log, so the steering is in both.
    let replayed = agentyk::messages_from_events(&events);
    assert_eq!(replayed, session.messages());
    Ok(())
}

#[tokio::test]
async fn input_pushed_between_turns_joins_the_next_one() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("ok")]))
        .build()?;

    let mut session = agent.session();
    // Nothing is running: a UI should not have to know that.
    session.input().push(Message::user("queued while idle"));
    session.run("go").await?;

    let types: Vec<String> = session
        .events()
        .await?
        .iter()
        .map(|event| event.event_type.clone())
        .collect();
    assert_eq!(
        types
            .iter()
            .filter(|t| *t == event_types::INPUT_MESSAGE)
            .count(),
        2
    );
    assert!(
        session
            .messages()
            .iter()
            .any(|message| message.text() == "queued while idle")
    );
    Ok(())
}
