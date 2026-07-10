//! `FileSystemCapability`: file tools reachable through a full agent turn.

use agentyk::{Agent, InMemoryFileSystem, ModelSpec, Result, SimDriver, SimTurn};
use serde_json::json;

#[tokio::test]
async fn write_then_read_round_trips_through_the_turn_loop() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call(
                "write_file",
                json!({"path": "notes.txt", "content": "remember the milk"}),
            ),
            SimTurn::tool_call("read_file", json!({"path": "notes.txt"})),
            SimTurn::text("done"),
        ]))
        .capability(agentyk::FileSystemCapability::new(InMemoryFileSystem::new()))
        .build()?;

    let mut session = agent.session();
    session.run("save and recall a note").await?;

    let events = session.events().await?;
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.data {
            agentyk::EventData::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, vec!["wrote notes.txt", "remember the milk"]);
    Ok(())
}

#[tokio::test]
async fn list_directory_reports_written_files() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("write_file", json!({"path": "a.txt", "content": "1"})),
            SimTurn::tool_call("list_directory", json!({})),
            SimTurn::text("done"),
        ]))
        .capability(agentyk::FileSystemCapability::new(InMemoryFileSystem::new()))
        .build()?;

    let mut session = agent.session();
    session.run("list the workspace").await?;

    let events = session.events().await?;
    let listing = events
        .iter()
        .find_map(|e| match &e.data {
            agentyk::EventData::ToolCompleted { name, output, .. } if name == "list_directory" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("a list_directory tool.completed event");
    assert!(listing.contains("a.txt (1 bytes)"));
    Ok(())
}

#[tokio::test]
async fn deleting_an_unknown_file_is_an_error_result_not_a_panic() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("delete_file", json!({"path": "missing.txt"})),
            SimTurn::text("done"),
        ]))
        .capability(agentyk::FileSystemCapability::new(InMemoryFileSystem::new()))
        .build()?;

    let mut session = agent.session();
    session.run("clean up").await?;
    let events = session.events().await?;
    let errored = events.iter().any(|e| {
        matches!(&e.data, agentyk::EventData::ToolCompleted { output, .. } if output.contains("no such file"))
    });
    assert!(errored);
    Ok(())
}
