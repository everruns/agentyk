//! `Capability::commands()` / `Session::execute_command`: host-invoked
//! slash commands that bypass the turn loop entirely.

use agentyk::{Agent, CommandContext, CommandDescriptor, ModelSpec, Result, SimDriver, ToolOutput};
use async_trait::async_trait;

struct GoalCapability;

#[async_trait]
impl agentyk::Capability for GoalCapability {
    fn id(&self) -> &str {
        "goal"
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor::new(
            "goal",
            "Set or show the session goal.",
        )]
    }

    async fn execute_command(
        &self,
        name: &str,
        args: &str,
        _context: &CommandContext,
    ) -> Option<ToolOutput> {
        if name != "goal" {
            return None;
        }
        Some(if args.trim().is_empty() {
            ToolOutput::text("no goal set")
        } else {
            ToolOutput::text(format!("goal set: {}", args.trim()))
        })
    }
}

#[tokio::test]
async fn commands_lists_every_capabilitys_descriptors() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .capability(GoalCapability)
        .build()?;

    let session = agent.session();
    let commands = session.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "goal");
    Ok(())
}

#[tokio::test]
async fn execute_command_dispatches_to_the_owning_capability() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .capability(GoalCapability)
        .build()?;

    let session = agent.session();
    let output = session.execute_command("goal", "ship it").await?;
    assert_eq!(output.content, "goal set: ship it");
    assert!(!output.is_error);

    // Bypasses the turn loop entirely: no event was recorded.
    assert!(session.events().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn unknown_command_is_an_error_result_not_a_panic() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .capability(GoalCapability)
        .build()?;

    let session = agent.session();
    let output = session.execute_command("nope", "").await?;
    assert!(output.is_error);
    assert!(output.content.contains("unknown command"));
    Ok(())
}
