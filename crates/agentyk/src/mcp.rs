//! MCP — Model Context Protocol support.
//!
//! [`McpCapability`] connects to an MCP server, discovers its tools, and
//! exposes them to the model as ordinary [`Tool`]s. Two transports:
//!
//! - **stdio** — a child process spoken to in newline-delimited JSON-RPC 2.0.
//!   Credentials go in its environment.
//! - **HTTP** (also needs feature `http`) — the Streamable HTTP transport, for
//!   servers you do not run yourself. Those are the ones that need
//!   [`McpAuthProvider`]: a hosted server authenticates per request, and a
//!   token that expires must be re-read rather than captured at connect time.
//!
//! Attach it like any other capability:
//!
//! ```no_run
//! use agentyk::{Agent, McpCapability, McpServer, ModelSpec};
//!
//! # fn demo() -> agentyk::Result<Agent> {
//! Agent::builder()
//!     .model(ModelSpec::llmsim())
//!     .capability(McpCapability::new(
//!         McpServer::stdio("github", "github-mcp-server").arg("stdio"),
//!     ))
//!     .build()
//! # }
//! ```
//!
//! The connection is lazy: the process is spawned (or the first HTTP request
//! sent) on the first turn that resolves tools.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, OnceCell, oneshot};

use agentyk_core::capability::Capability;
use agentyk_core::error::{Error, Result};
use agentyk_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How to reach an MCP server.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum McpTransport {
    /// A child process spoken to over stdio.
    Stdio {
        /// The executable to spawn.
        command: String,
        /// Arguments passed to it.
        args: Vec<String>,
        /// Environment variables set for it — where a locally-run server's
        /// credentials usually go.
        env: Vec<(String, String)>,
    },
    /// A remote server over the Streamable HTTP transport.
    Http {
        /// The MCP endpoint every JSON-RPC message is POSTed to.
        url: String,
        /// Extra headers sent with every request. Credentials belong in an
        /// [`McpAuthProvider`] instead, so they can be refreshed.
        headers: Vec<(String, String)>,
    },
}

/// Configuration for one MCP server.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct McpServer {
    /// Names the server in the capability id (`mcp:<name>`) and in errors.
    pub name: String,
    /// How to reach it.
    pub transport: McpTransport,
}

impl McpServer {
    /// A server spawned as a child process and spoken to over stdio.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: Vec::new(),
            },
        }
    }

    /// A remote server over HTTP. Needs feature `http` as well as `mcp` —
    /// without it, connecting reports that rather than silently doing
    /// nothing.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Http {
                url: url.into(),
                headers: Vec::new(),
            },
        }
    }

    /// Append one argument. Ignored for an HTTP server.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        if let McpTransport::Stdio { args, .. } = &mut self.transport {
            args.push(arg.into());
        }
        self
    }

    /// Append several arguments. Ignored for an HTTP server.
    pub fn args(mut self, new_args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        if let McpTransport::Stdio { args, .. } = &mut self.transport {
            args.extend(new_args.into_iter().map(Into::into));
        }
        self
    }

    /// Set an environment variable for the server process. Ignored for an
    /// HTTP server.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let McpTransport::Stdio { env, .. } = &mut self.transport {
            env.push((key.into(), value.into()));
        }
        self
    }

    /// Add a header sent with every HTTP request. Ignored for a stdio server.
    ///
    /// For credentials prefer [`McpAuthProvider`]: a header captured here is
    /// fixed for the life of the agent, which a token with an expiry is not.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let McpTransport::Http { headers, .. } = &mut self.transport {
            headers.push((key.into(), value.into()));
        }
        self
    }
}

/// Supplies the `Authorization` header for a remote MCP server.
///
/// A seam rather than a config field because the interesting cases are not
/// static: an OAuth access token expires, a workload identity is fetched from
/// a metadata service, a secret lives in a vault. The client asks **per
/// request**, so returning a fresh value is all a refreshing implementation
/// has to do.
///
/// ```
/// use agentyk::{McpAuthProvider, Result};
/// use async_trait::async_trait;
///
/// struct FromEnv;
///
/// #[async_trait]
/// impl McpAuthProvider for FromEnv {
///     async fn authorization(&self, server: &str) -> Result<Option<String>> {
///         let key = format!("MCP_{}_TOKEN", server.to_uppercase());
///         Ok(std::env::var(key).ok().map(|token| format!("Bearer {token}")))
///     }
/// }
/// ```
#[async_trait]
pub trait McpAuthProvider: Send + Sync {
    /// The full header value (`"Bearer …"`), or `None` to send none.
    /// Returning an error fails the request rather than retrying unauthorized.
    async fn authorization(&self, server: &str) -> Result<Option<String>>;
}

/// An [`McpAuthProvider`] that always returns the same bearer token — the
/// simple case, for a token that does not expire within the process's life.
pub struct StaticBearer(String);

impl StaticBearer {
    /// Authenticate with this token, sent as `Bearer <token>`.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

#[async_trait]
impl McpAuthProvider for StaticBearer {
    async fn authorization(&self, _server: &str) -> Result<Option<String>> {
        Ok(Some(format!("Bearer {}", self.0)))
    }
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// One way of moving JSON-RPC messages to a server and back.
///
/// The protocol above it — initialize, `tools/list`, `tools/call`, the
/// id/response correlation — is identical for every transport, so
/// [`McpClient`] owns all of it and a transport only moves bytes.
#[async_trait]
trait Transport: Send + Sync {
    /// Send a request and return its response envelope.
    async fn request(&self, id: i64, method: &str, params: Value) -> Result<Value>;
    /// Send a notification (no id, no response).
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
}

/// Minimal MCP client: initialize handshake, `tools/list`, `tools/call`,
/// over whichever transport the server is configured for.
pub struct McpClient {
    server_name: String,
    next_id: AtomicI64,
    transport: Box<dyn Transport>,
}

/// The stdio transport: newline-delimited JSON-RPC over a child process.
struct StdioTransport {
    server_name: String,
    pending: PendingMap,
    stdin: Mutex<ChildStdin>,
    _child: Child,
}

impl StdioTransport {
    async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self> {
        let mut command_builder = Command::new(command);
        let server = ServerLabel(name);
        let command_line = command;
        let mut command = command_builder
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in env {
            command = command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| {
            Error::Mcp(format!(
                "failed to spawn mcp server `{server}` ({command_line}): {e}"
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Mcp("mcp server stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Mcp("mcp server stdout unavailable".into()))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(target: "agentyk::mcp", %line, "unparseable mcp line");
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_i64) {
                    if let Some(sender) = reader_pending.lock().await.remove(&id) {
                        let _ = sender.send(value);
                    }
                } else {
                    tracing::trace!(target: "agentyk::mcp", "mcp notification ignored");
                }
            }
        });

        Ok(Self {
            server_name: name.to_string(),
            pending,
            stdin: Mutex::new(stdin),
            _child: child,
        })
    }

    async fn write_line(&self, payload: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let line = serde_json::to_string(payload)?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Mcp(format!("mcp write failed: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Mcp(format!("mcp write failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| Error::Mcp(format!("mcp flush failed: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&self, id: i64, method: &str, params: Value) -> Result<Value> {
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);

        self.write_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        tokio::time::timeout(REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| {
                Error::Mcp(format!(
                    "mcp `{}` timed out on `{method}`",
                    self.server_name
                ))
            })?
            .map_err(|_| Error::Mcp(format!("mcp `{}` closed", self.server_name)))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }
}

/// The Streamable HTTP transport.
///
/// Every JSON-RPC message is POSTed to one endpoint. The server may answer
/// with a JSON body or with an SSE stream — both are legal, and which one you
/// get can depend on the message — so the client accepts either and reads the
/// stream until the response to *this* id arrives.
#[cfg(feature = "http")]
mod http {
    use super::*;

    /// Header the server uses to bind subsequent requests to a session it
    /// created during `initialize`.
    const SESSION_HEADER: &str = "Mcp-Session-Id";
    const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";

    pub(super) struct HttpTransport {
        server_name: String,
        url: String,
        headers: Vec<(String, String)>,
        auth: Option<Arc<dyn McpAuthProvider>>,
        client: reqwest::Client,
        /// Learned from the `initialize` response, sent on everything after.
        session: Mutex<Option<String>>,
    }

    impl HttpTransport {
        pub(super) fn new(
            server_name: &str,
            url: &str,
            headers: &[(String, String)],
            auth: Option<Arc<dyn McpAuthProvider>>,
        ) -> Self {
            Self {
                server_name: server_name.to_string(),
                url: url.to_string(),
                headers: headers.to_vec(),
                auth,
                client: reqwest::Client::new(),
                session: Mutex::new(None),
            }
        }

        async fn send(&self, payload: Value) -> Result<reqwest::Response> {
            let mut builder = self
                .client
                .post(&self.url)
                .header("Content-Type", "application/json")
                // Both are legal responses; saying so lets the server pick.
                .header("Accept", "application/json, text/event-stream")
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION);
            for (key, value) in &self.headers {
                builder = builder.header(key, value);
            }
            if let Some(session) = self.session.lock().await.as_ref() {
                builder = builder.header(SESSION_HEADER, session);
            }
            // Asked per request, not captured at connect: that is what makes
            // an expiring token workable.
            if let Some(auth) = &self.auth
                && let Some(value) = auth.authorization(&self.server_name).await?
            {
                builder = builder.header("Authorization", value);
            }

            let response = builder.json(&payload).send().await.map_err(|error| {
                Error::Mcp(format!(
                    "mcp `{}` request failed: {error}",
                    self.server_name
                ))
            })?;

            if let Some(session) = response
                .headers()
                .get(SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
            {
                *self.session.lock().await = Some(session.to_string());
            }

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                // 401 is worth naming: it is almost always the auth provider,
                // not the protocol.
                let hint = match status.as_u16() {
                    401 | 403 => " — check the McpAuthProvider for this server",
                    _ => "",
                };
                return Err(Error::Mcp(format!(
                    "mcp `{}` returned {status}{hint}: {}",
                    self.server_name,
                    body.trim()
                )));
            }
            Ok(response)
        }

        /// Pull the JSON-RPC envelope with this id out of a response body,
        /// whether it arrived as JSON or as SSE events.
        fn extract(&self, id: i64, content_type: &str, body: &str) -> Result<Value> {
            if content_type.starts_with("text/event-stream") {
                for line in body.lines() {
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                        continue;
                    };
                    // A stream may carry server-initiated messages too; take
                    // the one answering this request.
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        return Ok(value);
                    }
                }
                return Err(Error::Mcp(format!(
                    "mcp `{}` stream ended without a response to request {id}",
                    self.server_name
                )));
            }
            serde_json::from_str(body).map_err(|error| {
                Error::Mcp(format!(
                    "mcp `{}` sent an unparseable response: {error}",
                    self.server_name
                ))
            })
        }
    }

    #[async_trait]
    impl Transport for HttpTransport {
        async fn request(&self, id: i64, method: &str, params: Value) -> Result<Value> {
            let response = self
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }))
                .await?;
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body = response.text().await.map_err(|error| {
                Error::Mcp(format!("mcp `{}` read failed: {error}", self.server_name))
            })?;
            self.extract(id, &content_type, &body)
        }

        async fn notify(&self, method: &str, params: Value) -> Result<()> {
            // A notification has no id and the server answers 202 with no
            // body; nothing to read.
            self.send(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }))
            .await
            .map(|_| ())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn transport() -> HttpTransport {
            HttpTransport::new("demo", "http://127.0.0.1:1/mcp", &[], None)
        }

        #[test]
        fn reads_the_matching_envelope_out_of_an_sse_stream() {
            let body =
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
            let value = transport()
                .extract(7, "text/event-stream", body)
                .expect("envelope found");
            assert_eq!(value["result"]["ok"], true);
        }

        #[test]
        fn ignores_stream_messages_addressed_to_someone_else() {
            let body = concat!(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"mine\":true}}\n\n",
            );
            let value = transport().extract(2, "text/event-stream", body).unwrap();
            assert_eq!(value["result"]["mine"], true);
        }

        #[test]
        fn a_stream_without_our_response_is_an_error_not_a_hang() {
            let error = transport()
                .extract(9, "text/event-stream", "data: {\"id\":1}\n\n")
                .expect_err("no response for 9");
            assert!(error.to_string().contains("without a response"), "{error}");
        }

        #[test]
        fn a_plain_json_body_is_read_directly() {
            let value = transport()
                .extract(1, "application/json", "{\"id\":1,\"result\":{\"ok\":1}}")
                .unwrap();
            assert_eq!(value["result"]["ok"], 1);
        }
    }
}

/// Formats a server name in errors without dragging the whole config in.
struct ServerLabel<'a>(&'a str);

impl std::fmt::Display for ServerLabel<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl McpClient {
    /// Connect to the server and complete the MCP initialize handshake.
    ///
    /// A stdio server's child process is killed when the client is dropped;
    /// an HTTP connection holds no process at all.
    pub async fn connect(server: &McpServer) -> Result<Self> {
        Self::connect_with_auth(server, None).await
    }

    /// Connect, authenticating an HTTP server with `auth` — see
    /// [`McpAuthProvider`]. Ignored for stdio, whose credentials are its
    /// environment.
    pub async fn connect_with_auth(
        server: &McpServer,
        auth: Option<Arc<dyn McpAuthProvider>>,
    ) -> Result<Self> {
        let transport: Box<dyn Transport> = match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                Box::new(StdioTransport::spawn(&server.name, command, args, env).await?)
            }
            #[cfg(feature = "http")]
            McpTransport::Http { url, headers } => {
                Box::new(http::HttpTransport::new(&server.name, url, headers, auth))
            }
            #[cfg(not(feature = "http"))]
            McpTransport::Http { .. } => {
                let _ = auth;
                return Err(Error::Mcp(format!(
                    "mcp server `{}` uses the HTTP transport, which needs the `http` feature \
                     alongside `mcp`",
                    server.name
                )));
            }
        };

        let client = Self {
            server_name: server.name.clone(),
            next_id: AtomicI64::new(1),
            transport,
        };
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "agentyk",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        client
            .transport
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// Send one request and unwrap its result, turning a JSON-RPC `error`
    /// into an [`Error::Mcp`].
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let response = self.transport.request(id, method, params).await?;
        if let Some(error) = response.get("error") {
            return Err(Error::Mcp(format!(
                "mcp `{}` `{method}` failed: {error}",
                self.server_name
            )));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Ask the server what tools it offers.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(parse_tool_list(&result))
    }

    /// Invoke one of the server's tools. Only text content blocks are read
    /// from the result.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        Ok(parse_tool_result(&result))
    }
}

pub(crate) fn parse_tool_list(result: &Value) -> Vec<ToolDefinition> {
    result["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    Some(ToolDefinition::new(
                        tool["name"].as_str()?,
                        tool["description"].as_str().unwrap_or_default(),
                        if tool["inputSchema"].is_object() {
                            tool["inputSchema"].clone()
                        } else {
                            json!({"type": "object"})
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_tool_result(result: &Value) -> ToolOutput {
    let content = result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| match block["type"].as_str() {
                    Some("text") => block["text"].as_str().map(str::to_string),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    ToolOutput::new(content, result["isError"].as_bool().unwrap_or(false))
}

struct McpTool {
    client: Arc<McpClient>,
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        match self
            .client
            .call_tool(&self.definition.name, arguments)
            .await
        {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// A capability exposing one MCP server's tools to the agent.
pub struct McpCapability {
    id: String,
    server: McpServer,
    auth: Option<Arc<dyn McpAuthProvider>>,
    client: OnceCell<Arc<McpClient>>,
}

impl McpCapability {
    /// Expose an MCP server's tools to the agent. The connection is made
    /// lazily, on the first turn that resolves tools.
    pub fn new(server: McpServer) -> Self {
        Self {
            id: format!("mcp:{}", server.name),
            server,
            auth: None,
            client: OnceCell::new(),
        }
    }

    /// Authenticate a remote server — see [`McpAuthProvider`]. No effect on
    /// a stdio server, whose credentials are its environment.
    ///
    /// ```no_run
    /// use agentyk::{McpCapability, McpServer, StaticBearer};
    ///
    /// let github = McpCapability::new(McpServer::http("github", "https://api.example/mcp"))
    ///     .auth(StaticBearer::new(std::env::var("GITHUB_TOKEN").unwrap()));
    /// ```
    pub fn auth(mut self, auth: impl McpAuthProvider + 'static) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// Authenticate with a provider you already hold as an `Arc` — the way to
    /// share one token source across several servers.
    pub fn auth_arc(mut self, auth: Arc<dyn McpAuthProvider>) -> Self {
        self.auth = Some(auth);
        self
    }
}

#[async_trait]
impl Capability for McpCapability {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "Tools provided by an MCP server."
    }

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        let client = self
            .client
            .get_or_try_init(|| async {
                McpClient::connect_with_auth(&self.server, self.auth.clone())
                    .await
                    .map(Arc::new)
            })
            .await?;
        let definitions = client.list_tools().await?;
        Ok(definitions
            .into_iter()
            .map(|definition| {
                Arc::new(McpTool {
                    client: client.clone(),
                    definition,
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_list() {
        let result = json!({
            "tools": [
                {"name": "search", "description": "Search things.", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}},
                {"name": "bare"},
            ]
        });
        let tools = parse_tool_list(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[1].parameters, json!({"type": "object"}));
    }

    #[test]
    fn parses_tool_result_text_blocks() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "image", "data": "…"},
                {"type": "text", "text": "line two"},
            ],
            "isError": false,
        });
        let output = parse_tool_result(&result);
        assert_eq!(output.content, "line one\nline two");
        assert!(!output.is_error);
    }
}
