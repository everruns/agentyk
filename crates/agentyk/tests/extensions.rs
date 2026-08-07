//! `ToolContext::extensions`: host-injected services reaching a tool.

use agentyk::{Agent, ModelSpec, Provider, Result, SimDriver, SimTurn, ToolOutput};
use serde_json::json;

#[derive(Clone)]
struct GreetingPrefix(String);

/// A tool that reads `GreetingPrefix` out of `ToolContext::extensions`.
struct Greeter;

#[async_trait::async_trait]
impl agentyk::Tool for Greeter {
    fn definition(&self) -> agentyk::ToolDefinition {
        agentyk::ToolDefinition::new(
            "greet",
            "Greet using the configured prefix.",
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        )
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        context: &agentyk::ToolContext,
    ) -> ToolOutput {
        let name = arguments["name"].as_str().unwrap_or("there");
        match context.extensions.get::<GreetingPrefix>() {
            Some(prefix) => ToolOutput::text(format!("{} {name}", prefix.0)),
            None => ToolOutput::error("no greeting prefix configured"),
        }
    }
}

#[tokio::test]
async fn extension_flows_through_to_tool_execution() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("greet", json!({"name": "world"})),
            SimTurn::text("done"),
        ])))
        .extension(GreetingPrefix("Yo,".to_string()))
        .tool(Greeter)
        .build()?;

    let mut session = agent.session();
    session.run("say hi").await?;

    let events = session.events().await?;
    let output = events
        .iter()
        .find_map(|e| match &e.data {
            agentyk::EventData::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(output, "Yo, world");
    Ok(())
}

#[tokio::test]
async fn missing_extension_is_visible_to_the_tool_not_a_panic() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("greet", json!({"name": "world"})),
            SimTurn::text("done"),
        ])))
        // No .extension(...) this time.
        .tool(Greeter)
        .build()?;

    let turn = agent.run("say hi").await?;
    assert_eq!(turn.response, "done");
    Ok(())
}
