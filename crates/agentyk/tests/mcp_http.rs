//! The HTTP MCP transport against a real socket: handshake, auth header,
//! session id, SSE responses, and tools reaching a turn.
#![cfg(all(feature = "mcp", feature = "http"))]

use std::sync::Arc;

use agentyk::{
    Agent, McpAuthProvider, McpCapability, McpClient, McpProtocolMode, McpServer, ModelSpec,
    Result, SimDriver, SimTurn, StaticBearer,
};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// A canned MCP server: answers each POST from a script, and keeps the raw
/// requests so a test can assert on headers and bodies.
struct FakeMcpServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    _handle: tokio::task::JoinHandle<()>,
}

struct FakeReply {
    status: &'static str,
    content_type: &'static str,
    body: String,
    session_id: Option<&'static str>,
}

impl FakeReply {
    fn ok(content_type: &'static str, body: String) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
            session_id: None,
        }
    }

    fn error(status: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
            session_id: None,
        }
    }

    fn session(mut self, session_id: &'static str) -> Self {
        self.session_id = Some(session_id);
        self
    }
}

impl FakeMcpServer {
    async fn serving(replies: Vec<FakeReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();

        let handle = tokio::spawn(async move {
            let mut replies = replies.into_iter();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer);
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("Content-Length: ")
                                    .or_else(|| line.strip_prefix("content-length: "))
                            })
                            .and_then(|value| value.trim().parse().ok())
                            .unwrap_or(0);
                        if rest.len() >= length {
                            break;
                        }
                    }
                }
                seen.lock()
                    .await
                    .push(String::from_utf8_lossy(&buffer).into_owned());

                let reply = replies
                    .next()
                    .unwrap_or_else(|| FakeReply::ok("application/json", "{}".into()));
                let session_header = reply
                    .session_id
                    .map(|id| format!("Mcp-Session-Id: {id}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\n{session_header}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.content_type,
                    reply.body.len(),
                    reply.body,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        Self {
            url: format!("http://127.0.0.1:{port}/mcp"),
            requests,
            _handle: handle,
        }
    }

    async fn requests(&self) -> Vec<String> {
        self.requests.lock().await.clone()
    }
}

fn envelope(id: u8, result: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// initialize → (notification) → tools/list → tools/call.
async fn scripted_server() -> FakeMcpServer {
    FakeMcpServer::serving(vec![
        FakeReply::ok(
            "application/json",
            envelope(1, serde_json::json!({"protocolVersion": "2025-06-18"})),
        )
        .session("session-42"),
        FakeReply::ok("application/json", String::from("{}")).session("session-42"),
        FakeReply::ok(
            "application/json",
            envelope(
                2,
                serde_json::json!({"tools": [{
                    "name": "search_issues",
                    "description": "Search issues.",
                    "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
                }]}),
            ),
        )
        .session("session-42"),
        // The call answers over SSE, which the transport must handle just as
        // well as a JSON body.
        FakeReply::ok(
            "text/event-stream",
            format!(
                "event: message\ndata: {}\n\n",
                envelope(
                    3,
                    serde_json::json!({"content": [{"type": "text", "text": "3 open issues"}]})
                )
            ),
        )
        .session("session-42"),
    ])
    .await
}

#[tokio::test]
async fn a_remote_server_contributes_its_tools_to_a_turn() -> Result<()> {
    let server = scripted_server().await;
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("search_issues", serde_json::json!({"q": "open"})),
            SimTurn::text("3 open issues"),
        ]))
        .capability(McpCapability::new(
            McpServer::http("github", &server.url).protocol_mode(McpProtocolMode::Stateful),
        ))
        .build()?;

    let mut session = agent.session();
    let turn = session.run("how many open issues?").await?;
    assert_eq!(turn.response, "3 open issues");

    let tool_output = session
        .events()
        .await?
        .into_iter()
        .find_map(|event| match event.data {
            agentyk::EventData::ToolCompleted { output, .. } => Some(output),
            _ => None,
        })
        .expect("tool.completed");
    assert_eq!(tool_output, "3 open issues", "the SSE response was read");
    Ok(())
}

#[tokio::test]
async fn the_session_id_from_initialize_is_sent_on_later_requests() -> Result<()> {
    let server = scripted_server().await;
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("done")]))
        .capability(McpCapability::new(
            McpServer::http("github", &server.url).protocol_mode(McpProtocolMode::Stateful),
        ))
        .build()?;
    agent.session().run("list the tools").await?;

    let requests = server.requests().await;
    assert!(requests.len() >= 3, "handshake then tools/list");
    assert!(
        !requests[0].contains("Mcp-Session-Id"),
        "nothing to send before initialize answered"
    );
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.contains("mcp-session-id: session-42")
                || request.contains("Mcp-Session-Id: session-42")),
        "every later request must carry the session: {requests:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_auth_provider_is_asked_for_every_request() -> Result<()> {
    /// Returns a different token each call, which is what an expiring
    /// credential looks like from the client's side.
    struct Rotating(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl McpAuthProvider for Rotating {
        async fn authorization(&self, server: &str) -> Result<Option<String>> {
            let nth = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(format!("Bearer {server}-{nth}")))
        }
    }

    let server = scripted_server().await;
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("done")]))
        .capability(
            McpCapability::new(
                McpServer::http("github", &server.url).protocol_mode(McpProtocolMode::Stateful),
            )
            .auth(Rotating(std::sync::atomic::AtomicUsize::new(0))),
        )
        .build()?;
    agent.session().run("list the tools").await?;

    let requests = server.requests().await;
    assert!(
        requests[0].contains("authorization: Bearer github-0"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].contains("authorization: Bearer github-1"),
        "a fresh token per request, not one captured at connect: {}",
        requests[1]
    );
    Ok(())
}

#[tokio::test]
async fn a_static_bearer_covers_the_simple_case() -> Result<()> {
    let server = scripted_server().await;
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("done")]))
        .capability(
            McpCapability::new(
                McpServer::http("github", &server.url).protocol_mode(McpProtocolMode::Stateful),
            )
            .auth(StaticBearer::new("ghp_secret")),
        )
        .build()?;
    agent.session().run("list the tools").await?;

    assert!(server.requests().await[0].contains("authorization: Bearer ghp_secret"));
    Ok(())
}

#[tokio::test]
async fn extra_headers_are_sent_and_credentials_are_not_required() -> Result<()> {
    let server = scripted_server().await;
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("done")]))
        .capability(McpCapability::new(
            McpServer::http("github", &server.url)
                .protocol_mode(McpProtocolMode::Stateful)
                .header("X-Tenant", "acme"),
        ))
        .build()?;
    agent.session().run("list the tools").await?;

    let first = server.requests().await[0].clone();
    assert!(first.contains("x-tenant: acme"), "{first}");
    assert!(!first.to_lowercase().contains("authorization:"));
    Ok(())
}

#[tokio::test]
async fn auto_uses_the_stateless_protocol_and_caches_the_verdict() -> Result<()> {
    let server = FakeMcpServer::serving(vec![
        FakeReply::ok(
            "application/json",
            envelope(
                1,
                serde_json::json!({"tools": [{
                    "name": "search",
                    "description": "Search.",
                    "inputSchema": {"type": "object"}
                }]}),
            ),
        ),
        FakeReply::ok(
            "application/json",
            envelope(
                2,
                serde_json::json!({
                    "content": [{"type": "text", "text": "found"}],
                    "isError": false
                }),
            ),
        ),
    ])
    .await;
    let client = McpClient::connect_with_auth(
        &McpServer::http("stateless", &server.url),
        Some(Arc::new(StaticBearer::new("oauth-token"))),
    )
    .await?;

    assert_eq!(client.list_tools().await?[0].name, "search");
    assert_eq!(
        client
            .call_tool("search", serde_json::json!({"q": "mcp"}))
            .await?
            .content,
        "found"
    );

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2, "no initialize round trip");
    for request in &requests {
        let lower = request.to_lowercase();
        assert!(lower.contains("mcp-protocol-version: 2026-07-28"));
        assert!(!lower.contains("mcp-session-id:"));
        assert!(!request.contains(r#""method":"initialize""#));
        assert!(request.contains("io.modelcontextprotocol/clientInfo"));
        assert!(request.contains(r#""name":"agentyk""#));
        assert!(
            request.contains(r#""io.modelcontextprotocol/protocolVersion":"2026-07-28""#),
            "without a handshake the version rides on every request: {request}"
        );
        assert!(
            request.contains(r#""io.modelcontextprotocol/clientCapabilities":{}"#),
            "we advertise no roots, sampling, or elicitation: {request}"
        );
        assert!(lower.contains("authorization: bearer oauth-token"));
    }
    assert!(
        requests[0]
            .to_lowercase()
            .contains("mcp-method: tools/list")
    );
    let call = requests[1].to_lowercase();
    assert!(call.contains("mcp-method: tools/call"));
    assert!(call.contains("mcp-name: search"));
    Ok(())
}

#[tokio::test]
async fn auto_falls_back_to_stateful_and_uses_the_server_version_and_session() -> Result<()> {
    let server = FakeMcpServer::serving(vec![
        FakeReply::error(
            "400 Bad Request",
            r#"{"error":{"message":"Unsupported protocol version"}}"#,
        ),
        FakeReply::ok(
            "application/json",
            envelope(0, serde_json::json!({"protocolVersion": "2025-03-26"})),
        )
        .session("legacy-session"),
        FakeReply::error("202 Accepted", ""),
        FakeReply::ok(
            "application/json",
            envelope(1, serde_json::json!({"tools": []})),
        ),
        FakeReply::ok(
            "application/json",
            envelope(2, serde_json::json!({"tools": []})),
        ),
    ])
    .await;
    let client = McpClient::connect(&McpServer::http("legacy", &server.url)).await?;

    assert!(client.list_tools().await?.is_empty());
    assert!(client.list_tools().await?.is_empty());

    let requests = server.requests().await;
    assert_eq!(requests.len(), 5);
    assert!(requests[0].contains(r#""method":"tools/list""#));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("mcp-protocol-version: 2026-07-28")
    );
    assert!(requests[1].contains(r#""method":"initialize""#));
    assert!(requests[2].contains(r#""method":"notifications/initialized""#));
    for request in &requests[2..] {
        let lower = request.to_lowercase();
        assert!(lower.contains("mcp-protocol-version: 2025-03-26"));
        assert!(lower.contains("mcp-session-id: legacy-session"));
    }
    assert!(
        requests[3..]
            .iter()
            .all(|request| request.contains(r#""method":"tools/list""#)),
        "the cached negotiation skips another probe and handshake"
    );
    Ok(())
}

#[tokio::test]
async fn pinning_the_latest_protocol_surfaces_rejection_without_falling_back() -> Result<()> {
    let server = FakeMcpServer::serving(vec![FakeReply::error(
        "400 Bad Request",
        r#"{"error":{"message":"Unsupported protocol version"}}"#,
    )])
    .await;
    let client = McpClient::connect(
        &McpServer::http("latest-only", &server.url).protocol_mode(McpProtocolMode::Latest),
    )
    .await?;

    let error = client
        .list_tools()
        .await
        .expect_err("the latest protocol is pinned");
    assert!(error.to_string().contains("400 Bad Request"), "{error}");
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        1,
        "a pinned stateless mode must not initialize"
    );
    assert!(
        requests[0]
            .to_lowercase()
            .contains("mcp-protocol-version: 2026-07-28")
    );
    Ok(())
}

/// A server that wants elicitation answered mid-call returns a result with no
/// `content` at all. Reading it as an ordinary result would tell the model the
/// tool succeeded and returned nothing.
#[tokio::test]
async fn a_tool_call_needing_input_fails_instead_of_returning_nothing() -> Result<()> {
    let server = FakeMcpServer::serving(vec![
        FakeReply::ok(
            "application/json",
            envelope(
                1,
                serde_json::json!({
                    "resultType": "complete",
                    "tools": [{"name": "deploy", "description": "Deploy."}],
                }),
            ),
        ),
        FakeReply::ok(
            "application/json",
            envelope(
                2,
                serde_json::json!({
                    "resultType": "input_required",
                    "inputRequests": [{
                        "method": "elicitation/create",
                        "params": {"message": "Which environment?"},
                    }],
                }),
            ),
        ),
    ])
    .await;
    let client = McpClient::connect(
        &McpServer::http("deployer", &server.url).protocol_mode(McpProtocolMode::Latest),
    )
    .await?;

    assert_eq!(client.list_tools().await?[0].name, "deploy");
    let error = client
        .call_tool("deploy", serde_json::json!({}))
        .await
        .expect_err("MRTR is not supported");
    assert!(error.to_string().contains("multi round-trip"), "{error}");
    Ok(())
}
