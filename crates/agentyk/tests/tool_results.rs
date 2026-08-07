//! Rich tool results: what the model sees, and what it sees on each protocol.

use agentyk::{
    Agent, ContentPart, EventData, ImageContentPart, ModelSpec, Provider, Result, SimDriver,
    SimTurn, Tool, ToolContext, ToolDefinition, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;

/// A tool that hands back an image alongside its text — a screenshot tool, a
/// chart renderer, a PDF page extractor.
struct ScreenshotTool;

#[async_trait]
impl Tool for ScreenshotTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("screenshot", "Capture the page.", json!({"type": "object"}))
    }

    async fn execute(&self, _arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        ToolOutput::text("captured the login page").with_parts([ContentPart::Image(
            ImageContentPart::from_base64("iVBORw0KGgo=", "image/png"),
        )])
    }
}

async fn run() -> Result<agentyk::Session> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("screenshot", json!({})),
            SimTurn::text("the login form has two fields"),
        ])))
        .tool(ScreenshotTool)
        .build()?;
    let mut session = agent.session();
    session.run("look at the page").await?;
    Ok(session)
}

#[tokio::test]
async fn an_image_result_enters_history_as_a_tool_message_part() -> Result<()> {
    let session = run().await?;
    let tool_result = session
        .messages()
        .iter()
        .find(|message| message.tool_call_id.is_some())
        .cloned()
        .expect("tool result message");

    assert_eq!(tool_result.text(), "captured the login page");
    assert!(
        matches!(tool_result.content.last(), Some(ContentPart::Image(_))),
        "the image must reach the model: {:?}",
        tool_result.content
    );
    Ok(())
}

#[tokio::test]
async fn image_parts_survive_a_replay_of_the_log() -> Result<()> {
    let session = run().await?;
    let events = session.events().await?;

    // Recorded on tool.completed…
    let recorded = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::ToolCompleted { parts, .. } => Some(parts.clone()),
            _ => None,
        })
        .expect("tool.completed recorded");
    assert_eq!(recorded.len(), 1);

    // …and therefore reconstructed by a fold of the log alone.
    let replayed = agentyk::messages_from_events(&events);
    let tool_result = replayed
        .iter()
        .find(|message| message.tool_call_id.is_some())
        .expect("replayed tool result");
    assert_eq!(tool_result.content, session.messages()[2].content);
    Ok(())
}

#[tokio::test]
async fn a_turn_can_be_opened_with_an_image() -> Result<()> {
    use agentyk::{Message, SimDriver};

    // The other half of multimodal: a user asking about a screenshot, not a
    // tool returning one. `run` takes anything that becomes a `Message`, so
    // this needs no separate entry point.
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "a login form",
        )])))
        .build()?;

    let mut session = agent.session();
    let turn = session
        .run(Message::user_multimodal(vec![
            ContentPart::text("what is this?"),
            ContentPart::Image(ImageContentPart::from_base64("iVBORw0KGgo=", "image/png")),
        ]))
        .await?;
    assert_eq!(turn.response, "a login form");

    let input = session.messages().first().cloned().expect("input recorded");
    assert_eq!(input.text(), "what is this?");
    assert!(
        matches!(input.content.last(), Some(ContentPart::Image(_))),
        "the image is what the model was asked about: {:?}",
        input.content
    );
    Ok(())
}

#[tokio::test]
async fn a_plain_string_still_opens_a_turn() -> Result<()> {
    use agentyk::SimDriver;

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        // One scripted turn per session below.
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::text("hi"),
            SimTurn::text("hi"),
        ])))
        .build()?;
    assert_eq!(agent.session().run("hello").await?.response, "hi");
    // …and an owned String, which is what a host that formats a prompt has.
    assert_eq!(
        agent.session().run(format!("hel{}", "lo")).await?.response,
        "hi"
    );
    Ok(())
}
