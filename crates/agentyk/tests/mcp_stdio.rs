//! End-to-end test of the MCP stdio transport against a canned shell-script
//! server. The client assigns JSON-RPC ids deterministically (1, 2, 3, …), so
//! a scripted server can answer with matching ids — this exercises the real
//! spawn/handshake/list/call path without an external MCP binary.

#![cfg(all(unix, feature = "mcp"))]

use agentyk::{Agent, EventData, McpCapability, McpServer, ModelSpec, Result, SimDriver, SimTurn};
use serde_json::json;

/// Reads one request line, answers with the canned response; repeats.
/// Sequence: initialize (id 1) → initialized notification (no reply) →
/// tools/list (id 2) → tools/call (id 3).
const FAKE_MCP_SERVER: &str = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
read line
read line
echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"lookup","description":"Look things up.","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}'
read line
echo '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"lookup says: 42"}],"isError":false}}'
"#;

#[tokio::test]
async fn mcp_tools_flow_through_a_turn() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("lookup", json!({"q": "answer"})),
            SimTurn::text("The answer is 42."),
        ]))
        .capability(McpCapability::new(
            McpServer::stdio("fake", "sh")
                .arg("-c")
                .arg(FAKE_MCP_SERVER),
        ))
        .build()?;

    let mut session = agent.session();
    let turn = session.run("look it up").await?;

    assert_eq!(turn.response, "The answer is 42.");
    assert_eq!(turn.tool_calls, 1);

    // The MCP tool result made it into the event log.
    let events = session.events().await?;
    assert!(events.iter().any(|e| matches!(
        &e.data,
        EventData::ToolCompleted { name, output, is_error, .. }
            if name == "lookup" && output == "lookup says: 42" && !is_error
    )));
    Ok(())
}
