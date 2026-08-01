//! End-to-end test of the MCP stdio transport against a canned shell-script
//! server. The client assigns JSON-RPC ids deterministically (1, 2, 3, …), so
//! a scripted server can answer with matching ids — this exercises the real
//! spawn/probe/handshake/list/call path without an external MCP binary.
//!
//! Both eras get a server here. stdio has no status codes to drive a
//! fallback, so the client opens with a `server/discover` probe: a modern
//! server answers it and is then spoken to statelessly, while a legacy
//! server rejects the unknown method and gets the `initialize` handshake.

#![cfg(all(unix, feature = "mcp"))]

use agentyk::{
    Agent, DynamicMcpCapability, EventData, McpCapability, McpClient, McpServer, ModelSpec, Result,
    SimDriver, SimTurn,
};
use serde_json::json;

/// Reads one request line, answers with the canned response; repeats. Each
/// line is also appended to `$MCP_LOG` so a test can assert on what we sent.
///
/// Sequence: server/discover (id 1, refused) → initialize (id 2) →
/// initialized notification (no reply) → tools/list (id 3) → tools/call
/// (id 4).
const LEGACY_MCP_SERVER: &str = r#"
log() { [ -n "$MCP_LOG" ] && printf '%s\n' "$1" >> "$MCP_LOG"; }
read line; log "$line"
echo '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}'
read line; log "$line"
echo '{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
read line; log "$line"
read line; log "$line"
echo '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"lookup","description":"Look things up.","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}'
read line; log "$line"
echo '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"lookup says: 42"}],"isError":false}}'
"#;

/// A `2026-07-28` server: answers the probe, then serves statelessly with no
/// handshake at all.
///
/// Sequence: server/discover (id 1) → tools/list (id 2) → tools/call (id 3).
const MODERN_MCP_SERVER: &str = r#"
log() { [ -n "$MCP_LOG" ] && printf '%s\n' "$1" >> "$MCP_LOG"; }
read line; log "$line"
echo '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"instructions":"Look things up.","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fake","version":"7"}}}}'
read line; log "$line"
echo '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"lookup","description":"Look things up.","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}'
read line; log "$line"
echo '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"lookup says: 42"}],"isError":false}}'
"#;

/// A configurable modern server used to prove that a retained dynamic
/// capability handle changes the next turn without replacing the session.
const RELOADABLE_MCP_SERVER: &str = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read line
printf '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"%s","description":"Reloadable tool.","inputSchema":{"type":"object"}}]}}\n' "$MCP_TOOL"
read line
printf '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"%s"}],"isError":false}}\n' "$MCP_RESULT"
"#;

/// A log file the scripted server appends each request line to.
struct RequestLog(std::path::PathBuf);

impl RequestLog {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("agentyk-mcp-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn path(&self) -> String {
        self.0.display().to_string()
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn run_a_turn(script: &str, log: &RequestLog) -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("lookup", json!({"q": "answer"})),
            SimTurn::text("The answer is 42."),
        ]))
        .capability(McpCapability::new(
            McpServer::stdio("fake", "sh")
                .arg("-c")
                .arg(script)
                .env("MCP_LOG", log.path()),
        ))
        .build()?;

    let mut session = agent.session();
    let turn = session.run("look it up").await?;
    assert_eq!(turn.response, "The answer is 42.");
    assert_eq!(turn.tool_calls, 1);

    let events = session.events().await?;
    assert!(events.iter().any(|e| matches!(
        &e.data,
        EventData::ToolCompleted { name, output, is_error, .. }
            if name == "lookup" && output == "lookup says: 42" && !is_error
    )));
    Ok(())
}

#[tokio::test]
async fn a_legacy_stdio_server_still_gets_its_handshake() -> Result<()> {
    let log = RequestLog::new("legacy");
    run_a_turn(LEGACY_MCP_SERVER, &log).await?;

    let sent = log.lines();
    assert!(
        sent[0].contains(r#""method":"server/discover""#),
        "the probe opens the connection: {:?}",
        sent[0]
    );
    assert!(
        sent[1].contains(r#""method":"initialize""#),
        "refusing the probe identifies a server that wants the handshake: {:?}",
        sent[1]
    );
    assert!(sent[2].contains("notifications/initialized"));
    Ok(())
}

#[tokio::test]
async fn a_modern_stdio_server_is_spoken_to_statelessly() -> Result<()> {
    let log = RequestLog::new("modern");
    run_a_turn(MODERN_MCP_SERVER, &log).await?;

    let sent = log.lines();
    assert!(sent[0].contains(r#""method":"server/discover""#));
    assert!(
        !sent.iter().any(|line| line.contains(r#""initialize""#)),
        "answering the probe means no handshake at all: {sent:?}"
    );
    for line in &sent[1..] {
        assert!(
            line.contains(r#""io.modelcontextprotocol/protocolVersion":"2026-07-28""#),
            "every stateless request carries its version: {line}"
        );
        assert!(line.contains(r#""io.modelcontextprotocol/clientCapabilities":{}"#));
    }
    Ok(())
}

#[tokio::test]
async fn dynamic_servers_change_at_the_next_turn_boundary() -> Result<()> {
    let dynamic = DynamicMcpCapability::new();
    dynamic.activate(
        McpServer::stdio("first", "sh")
            .arg("-c")
            .arg(RELOADABLE_MCP_SERVER)
            .env("MCP_TOOL", "first_tool")
            .env("MCP_RESULT", "first result"),
    );
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("first_tool", json!({})),
            SimTurn::text("first done"),
            SimTurn::tool_call("second_tool", json!({})),
            SimTurn::text("second done"),
        ]))
        .capability(dynamic.clone())
        .build()?;
    let mut session = agent.session();

    assert_eq!(session.run("first").await?.response, "first done");
    dynamic.replace_servers([McpServer::stdio("second", "sh")
        .arg("-c")
        .arg(RELOADABLE_MCP_SERVER)
        .env("MCP_TOOL", "second_tool")
        .env("MCP_RESULT", "second result")]);
    assert_eq!(dynamic.server_names(), vec!["second"]);
    assert_eq!(session.run("second").await?.response, "second done");

    let completed: Vec<_> = session
        .events()
        .await?
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ToolCompleted { name, output, .. } => Some((name, output)),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        [
            ("first_tool".into(), "first result".into()),
            ("second_tool".into(), "second result".into())
        ]
    );
    Ok(())
}

/// Answers `server/discover` twice: once for the connect-time probe, once
/// for the explicit call the test makes.
const DISCOVERABLE_MCP_SERVER: &str = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"instructions":"Look things up.","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fake","version":"7"}}}}'
read line
echo '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","supportedVersions":["2026-07-28","2025-11-25"],"capabilities":{"tools":{}},"instructions":"Look things up.","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fake","version":"7"}}}}'
"#;

#[tokio::test]
async fn discover_reports_what_the_server_says_about_itself() -> Result<()> {
    let client = McpClient::connect(
        &McpServer::stdio("fake", "sh")
            .arg("-c")
            .arg(DISCOVERABLE_MCP_SERVER),
    )
    .await?;

    let discovery = client.discover().await?;
    assert_eq!(discovery.supported_versions, ["2026-07-28", "2025-11-25"]);
    assert_eq!(discovery.capabilities["tools"], json!({}));
    assert_eq!(discovery.instructions.as_deref(), Some("Look things up."));
    assert_eq!(
        discovery.server_info.as_ref().map(|info| &info["name"]),
        Some(&json!("fake")),
        "self-reported, and only good for display"
    );
    Ok(())
}
