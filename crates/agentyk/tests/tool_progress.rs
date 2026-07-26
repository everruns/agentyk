//! What a long-running tool can tell the host while it runs, and what it can
//! hand back beyond a string.

use std::sync::{Arc, Mutex};

use agentyk::{
    Agent, Event, EventData, EventListener, ModelSpec, Result, SimDriver, SimTurn, Tool,
    ToolContext, ToolDefinition, ToolOutput, ToolProgress, event_types,
};
use async_trait::async_trait;
use serde_json::json;

/// A tool that narrates two steps and then returns both a model-facing
/// string and host-facing structure — the shape a `bash`-style tool wants.
struct BuildTool;

#[async_trait]
impl Tool for BuildTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("build", "Build the project.", json!({"type": "object"}))
    }

    async fn execute(&self, _arguments: serde_json::Value, context: &ToolContext) -> ToolOutput {
        context
            .report_progress(ToolProgress::message("compiling agentyk-core"))
            .await;
        context
            .report_progress(
                ToolProgress::message("compiling agentyk").with_detail(json!({"percent": 60})),
            )
            .await;
        ToolOutput::text("build succeeded in 3.2s")
            .with_metadata(json!({"exit_code": 0, "duration_ms": 3200}))
    }
}

#[derive(Default)]
struct Collector(Mutex<Vec<Event>>);

#[async_trait]
impl EventListener for Collector {
    async fn on_event(&self, event: &Event) {
        self.0.lock().expect("collector").push(event.clone());
    }

    fn name(&self) -> &'static str {
        "collector"
    }
}

async fn run_build() -> Result<(Arc<Collector>, agentyk::Session)> {
    let collector = Arc::new(Collector::default());
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("build", json!({})),
            SimTurn::text("built it"),
        ]))
        .tool(BuildTool)
        .listener_arc(collector.clone())
        .build()?;
    let mut session = agent.session();
    session.run("build the project").await?;
    Ok((collector, session))
}

#[tokio::test]
async fn progress_reaches_listeners_in_order_while_the_tool_runs() -> Result<()> {
    let (collector, _session) = run_build().await?;
    let events = collector.0.lock().expect("collector").clone();

    let reported: Vec<String> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ToolProgress { progress, .. } => Some(progress.message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reported, ["compiling agentyk-core", "compiling agentyk"]);

    // Progress lands between started and completed, which is the whole point:
    // it fills the window a user is waiting through.
    let position = |kind: &str| {
        events
            .iter()
            .position(|event| event.event_type == kind)
            .unwrap_or_else(|| panic!("no {kind} event"))
    };
    assert!(position(event_types::TOOL_STARTED) < position(event_types::TOOL_PROGRESS));
    assert!(position(event_types::TOOL_PROGRESS) < position(event_types::TOOL_COMPLETED));

    // Structured detail survives the trip to the listener.
    let detail = events.iter().find_map(|event| match &event.data {
        EventData::ToolProgress { progress, .. } if !progress.detail.is_null() => {
            Some(progress.detail.clone())
        }
        _ => None,
    });
    assert_eq!(detail, Some(json!({"percent": 60})));
    Ok(())
}

#[tokio::test]
async fn progress_is_ephemeral_and_never_persisted() -> Result<()> {
    let (_collector, session) = run_build().await?;
    let logged = session.events().await?;
    assert!(
        !logged
            .iter()
            .any(|event| event.event_type == event_types::TOOL_PROGRESS),
        "progress must not reach the event log"
    );
    // …and therefore cannot appear in a replayed history either.
    assert!(
        !session
            .messages()
            .iter()
            .any(|message| message.text().contains("compiling")),
        "progress must not become conversation history"
    );
    Ok(())
}

#[tokio::test]
async fn result_metadata_reaches_the_host_but_not_the_model() -> Result<()> {
    let (_collector, session) = run_build().await?;
    let completed = session
        .events()
        .await?
        .into_iter()
        .find_map(|event| match event.data {
            EventData::ToolCompleted {
                output, metadata, ..
            } => Some((output, metadata)),
            _ => None,
        })
        .expect("tool.completed recorded");

    assert_eq!(completed.0, "build succeeded in 3.2s");
    assert_eq!(completed.1, json!({"exit_code": 0, "duration_ms": 3200}));

    // The model's view is the string alone — metadata never enters history.
    let tool_result = session
        .messages()
        .iter()
        .find(|message| message.tool_call_id.is_some())
        .expect("tool result message");
    assert_eq!(tool_result.text(), "build succeeded in 3.2s");
    assert!(!tool_result.text().contains("exit_code"));
    Ok(())
}

#[tokio::test]
async fn a_tool_can_report_progress_with_no_host_listening() -> Result<()> {
    // The no-op path: a unit test constructs a bare context and the tool must
    // still run, not panic or block.
    let context = ToolContext::new(agentyk::SessionId::new(), agentyk::TurnId::new());
    let output = BuildTool.execute(json!({}), &context).await;
    assert!(!output.is_error);
    Ok(())
}
