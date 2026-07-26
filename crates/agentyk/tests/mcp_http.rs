//! The HTTP MCP transport against a real socket: handshake, auth header,
//! session id, SSE responses, and tools reaching a turn.
#![cfg(all(feature = "mcp", feature = "http"))]

use std::sync::Arc;

use agentyk::{
    Agent, McpAuthProvider, McpCapability, McpServer, ModelSpec, Result, SimDriver, SimTurn,
    StaticBearer,
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

impl FakeMcpServer {
    /// `replies` are `(content_type, body)` pairs, served in order.
    async fn serving(replies: Vec<(&'static str, String)>) -> Self {
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

                let (content_type, body) = replies
                    .next()
                    .unwrap_or(("application/json", String::from("{}")));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nMcp-Session-Id: session-42\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
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
        (
            "application/json",
            envelope(1, serde_json::json!({"protocolVersion": "2025-06-18"})),
        ),
        ("application/json", String::from("{}")),
        (
            "application/json",
            envelope(
                2,
                serde_json::json!({"tools": [{
                    "name": "search_issues",
                    "description": "Search issues.",
                    "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
                }]}),
            ),
        ),
        // The call answers over SSE, which the transport must handle just as
        // well as a JSON body.
        (
            "text/event-stream",
            format!(
                "event: message\ndata: {}\n\n",
                envelope(
                    3,
                    serde_json::json!({"content": [{"type": "text", "text": "3 open issues"}]})
                )
            ),
        ),
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
        .capability(McpCapability::new(McpServer::http("github", &server.url)))
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
        .capability(McpCapability::new(McpServer::http("github", &server.url)))
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
            McpCapability::new(McpServer::http("github", &server.url))
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
            McpCapability::new(McpServer::http("github", &server.url))
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
            McpServer::http("github", &server.url).header("X-Tenant", "acme"),
        ))
        .build()?;
    agent.session().run("list the tools").await?;

    let first = server.requests().await[0].clone();
    assert!(first.contains("x-tenant: acme"), "{first}");
    assert!(!first.to_lowercase().contains("authorization:"));
    Ok(())
}
