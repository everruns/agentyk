//! End-to-end coverage for durable, tool-authored lifecycle narration.

use agentyk::{
    Agent, EventData, ModelSpec, Provider, Result, SimDriver, SimTurn, Tool, ToolCall,
    ToolCallDecision, ToolContext, ToolDefinition, ToolInvocation, ToolNarrationContext,
    ToolNarrationPhase, ToolOutput, TurnMiddleware,
};
use async_trait::async_trait;
use serde_json::{Value, json};

struct NarratedTool;

struct RedirectLibrary;

#[async_trait]
impl TurnMiddleware for RedirectLibrary {
    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if invocation.call.arguments["place"] == "old library" {
            let mut call = invocation.call.clone();
            call.arguments["place"] = json!("library");
            ToolCallDecision::Rewrite(call)
        } else {
            ToolCallDecision::Proceed
        }
    }
}

#[async_trait]
impl Tool for NarratedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("visit", "Visit a named place.", json!({"type": "object"}))
    }

    fn display_name(&self) -> Option<&str> {
        Some("Visit place")
    }

    fn narrate(
        &self,
        call: &ToolCall,
        phase: ToolNarrationPhase,
        _context: ToolNarrationContext<'_>,
    ) -> Option<String> {
        let place = call.arguments["place"].as_str()?;
        let verb = match phase {
            ToolNarrationPhase::Started => "Visiting",
            ToolNarrationPhase::Waiting => "Waiting to visit",
            ToolNarrationPhase::Completed => "Visited",
            ToolNarrationPhase::Failed => "Failed to visit",
        };
        Some(format!("{verb} {place}"))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        if arguments["fail"].as_bool().unwrap_or(false) {
            ToolOutput::error("closed")
        } else {
            ToolOutput::text("open")
        }
    }
}

#[tokio::test]
async fn tool_narration_is_durable_for_success_and_failure() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("visit", json!({"place": "old library"})),
            SimTurn::text("done"),
            SimTurn::tool_call("visit", json!({"place": "museum", "fail": true})),
            SimTurn::text("could not visit"),
        ])))
        .tool(NarratedTool)
        .middleware(RedirectLibrary)
        .build()?;
    let mut session = agent.session();

    session.run("visit the library").await?;
    session.run("visit the museum").await?;

    let narration: Vec<_> = session
        .events()
        .await?
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ToolStarted {
                display_name,
                narration,
                ..
            }
            | EventData::ToolCompleted {
                display_name,
                narration,
                ..
            } => Some((display_name, narration)),
            _ => None,
        })
        .collect();

    assert_eq!(
        narration,
        [
            (Some("Visit place".into()), Some("Visiting library".into())),
            (Some("Visit place".into()), Some("Visited library".into())),
            (Some("Visit place".into()), Some("Visiting museum".into())),
            (
                Some("Visit place".into()),
                Some("Failed to visit museum".into())
            ),
        ]
    );
    Ok(())
}
