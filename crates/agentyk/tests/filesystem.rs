//! `FileSystemCapability`: file tools reachable through a full agent turn.
#![cfg(feature = "fs")]

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

/// Drive one tool call through a real turn and return what the model saw plus
/// the structured metadata the host saw.
async fn call_tool(
    store: std::sync::Arc<InMemoryFileSystem>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool, serde_json::Value)> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call(name, arguments),
            SimTurn::text("done"),
        ]))
        .capability(agentyk::FileSystemCapability::from_arc(store))
        .build()?;
    let mut session = agent.session();
    session.run("do it").await?;
    let completed = session
        .events()
        .await?
        .into_iter()
        .find_map(|event| match event.data {
            agentyk::EventData::ToolCompleted {
                output,
                is_error,
                metadata,
                ..
            } => Some((output, is_error, metadata)),
            _ => None,
        })
        .expect("tool.completed recorded");
    Ok(completed)
}

async fn store_with(files: &[(&str, &str)]) -> std::sync::Arc<InMemoryFileSystem> {
    use agentyk::FileSystem;
    let store = std::sync::Arc::new(InMemoryFileSystem::new());
    for (path, content) in files {
        store.write_file(path, content).await.expect("seed file");
    }
    store
}

#[tokio::test]
async fn edit_file_refuses_an_ambiguous_match_and_changes_nothing() -> Result<()> {
    use agentyk::FileSystem;
    let store = store_with(&[("a.rs", "let x = 1;\nlet x = 1;\n")]).await;
    let (output, is_error, _) = call_tool(
        store.clone(),
        "edit_file",
        json!({"path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"}),
    )
    .await?;

    assert!(is_error, "{output}");
    assert!(output.contains("appears 2 times"), "{output}");
    assert_eq!(
        store.read_file("a.rs").await.unwrap(),
        "let x = 1;\nlet x = 1;\n",
        "a refused edit must not touch the file"
    );
    Ok(())
}

#[tokio::test]
async fn edit_file_applies_a_unique_match_and_reports_it() -> Result<()> {
    use agentyk::FileSystem;
    let store = store_with(&[("a.rs", "fn main() {}\n")]).await;
    let (output, is_error, metadata) = call_tool(
        store.clone(),
        "edit_file",
        json!({"path": "a.rs", "old_string": "fn main", "new_string": "pub fn main"}),
    )
    .await?;

    assert!(!is_error, "{output}");
    assert_eq!(store.read_file("a.rs").await.unwrap(), "pub fn main() {}\n");
    assert_eq!(metadata["replacements"], 1);
    Ok(())
}

#[tokio::test]
async fn edit_file_replaces_every_occurrence_when_asked() -> Result<()> {
    use agentyk::FileSystem;
    let store = store_with(&[("a.rs", "old\nold\n")]).await;
    let (_, is_error, metadata) = call_tool(
        store.clone(),
        "edit_file",
        json!({"path": "a.rs", "old_string": "old", "new_string": "new", "replace_all": true}),
    )
    .await?;

    assert!(!is_error);
    assert_eq!(store.read_file("a.rs").await.unwrap(), "new\nnew\n");
    assert_eq!(metadata["replacements"], 2);
    Ok(())
}

#[tokio::test]
async fn grep_files_searches_recursively_and_reports_path_and_line() -> Result<()> {
    let store = store_with(&[
        ("src/main.rs", "fn main() {}\n"),
        ("src/deep/lib.rs", "// nothing\nfn helper() {}\n"),
        ("README.md", "prose\n"),
    ])
    .await;

    let (output, is_error, metadata) =
        call_tool(store, "grep_files", json!({"pattern": "^fn "})).await?;

    assert!(!is_error, "{output}");
    assert!(output.contains("src/main.rs:1: fn main() {}"), "{output}");
    assert!(
        output.contains("src/deep/lib.rs:2: fn helper() {}"),
        "nested files must be searched too: {output}"
    );
    assert!(!output.contains("README.md"), "{output}");
    assert_eq!(metadata["matches"], 2);
    Ok(())
}

#[tokio::test]
async fn grep_files_can_be_scoped_to_a_subdirectory() -> Result<()> {
    let store = store_with(&[("src/a.rs", "target\n"), ("docs/b.md", "target\n")]).await;
    let (output, _, metadata) = call_tool(
        store,
        "grep_files",
        json!({"pattern": "target", "path": "docs"}),
    )
    .await?;
    assert!(output.contains("docs/b.md"), "{output}");
    assert_eq!(metadata["matches"], 1, "{output}");
    Ok(())
}

#[tokio::test]
async fn an_invalid_pattern_is_reported_to_the_model_not_the_turn() -> Result<()> {
    let store = store_with(&[("a.rs", "x\n")]).await;
    let (output, is_error, _) = call_tool(store, "grep_files", json!({"pattern": "("})).await?;
    assert!(is_error);
    assert!(output.contains("invalid pattern"), "{output}");
    Ok(())
}

#[tokio::test]
async fn stat_file_answers_for_files_directories_and_missing_paths() -> Result<()> {
    let store = store_with(&[("src/main.rs", "12345")]).await;

    let (file, is_error, metadata) =
        call_tool(store.clone(), "stat_file", json!({"path": "src/main.rs"})).await?;
    assert!(!is_error, "{file}");
    assert_eq!(metadata["size"], 5);
    assert_eq!(metadata["is_dir"], false);

    let (dir, _, metadata) = call_tool(store.clone(), "stat_file", json!({"path": "src"})).await?;
    assert!(dir.contains("directory"), "{dir}");
    assert_eq!(metadata["is_dir"], true);

    // Missing is an answer, not a failure.
    let (missing, is_error, metadata) =
        call_tool(store, "stat_file", json!({"path": "nope.rs"})).await?;
    assert!(!is_error, "{missing}");
    assert!(missing.contains("does not exist"), "{missing}");
    assert_eq!(metadata["exists"], false);
    Ok(())
}

#[tokio::test]
async fn read_file_can_return_a_line_window_and_says_which_one() -> Result<()> {
    let store = store_with(&[("a.txt", "one\ntwo\nthree\nfour\n")]).await;
    let (output, is_error, metadata) = call_tool(
        store,
        "read_file",
        json!({"path": "a.txt", "offset": 2, "limit": 2}),
    )
    .await?;

    assert!(!is_error, "{output}");
    assert_eq!(output, "two\nthree");
    assert_eq!(metadata["lines"]["offset"], 2);
    assert_eq!(metadata["lines"]["returned"], 2);
    assert_eq!(metadata["lines"]["total"], 4);
    Ok(())
}

#[tokio::test]
async fn mutating_tools_declare_themselves_in_the_hints_hatch() -> Result<()> {
    use agentyk::Capability;
    let capability = agentyk::FileSystemCapability::new(InMemoryFileSystem::new());
    let tools = capability.tools().await?;
    let hint = |name: &str, key: &str| -> bool {
        tools
            .iter()
            .map(|tool| tool.definition())
            .find(|definition| definition.name == name)
            .map(|definition| definition.metadata["hints"][key] == json!(true))
            .unwrap_or_else(|| panic!("no {name} tool"))
    };

    assert!(hint("read_file", "readonly"));
    assert!(hint("grep_files", "readonly"));
    assert!(hint("stat_file", "readonly"));
    assert!(hint("list_directory", "readonly"));
    assert!(hint("edit_file", "destructive"));
    assert!(hint("write_file", "destructive"));
    assert!(hint("delete_file", "destructive"));
    Ok(())
}
