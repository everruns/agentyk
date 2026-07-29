//! MCP — Model Context Protocol support.
//!
//! [`McpCapability`] connects to an MCP server, discovers its tools, and
//! exposes them to the model as ordinary [`Tool`]s. Two transports:
//!
//! - **stdio** — a child process spoken to in newline-delimited JSON-RPC 2.0.
//!   Credentials go in its environment.
//! - **HTTP** (also needs feature `http`) — the Streamable HTTP transport, for
//!   servers you do not run yourself. [`McpProtocolMode::Auto`] tries the
//!   stateless `2026-07-28` protocol first and falls back to a stateful
//!   handshake when required. Hosted servers use [`McpAuthProvider`] per
//!   request, so an expiring token can be refreshed without reconnecting.
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
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, OnceCell, oneshot};

use agentyk_core::capability::Capability;
use agentyk_core::error::{Error, Result};
use agentyk_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the stdio era probe waits before concluding the server is not
/// answering `server/discover`. Short on purpose: it is paid once per
/// connect, and only a server that ignores unknown methods outright — rather
/// than erroring on them — makes us wait it out.
const DISCOVER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "mcp-oauth")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp-oauth")))]
pub mod oauth;
/// MCP protocol versions and HTTP negotiation policy.
pub mod protocol;

pub use protocol::McpProtocolMode;
use protocol::{MCP_PROTOCOL_VERSION_LATEST, Negotiated};

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
    /// Which protocol era to speak. Both transports honour it; they differ
    /// only in how [`McpProtocolMode::Auto`] probes for the answer.
    pub protocol_mode: McpProtocolMode,
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
            protocol_mode: McpProtocolMode::Auto,
        }
    }

    /// A remote server over HTTP, using [`McpProtocolMode::Auto`].
    ///
    /// Needs feature `http` as well as `mcp`; without it, connecting reports
    /// that rather than silently doing nothing.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Http {
                url: url.into(),
                headers: Vec::new(),
            },
            protocol_mode: McpProtocolMode::Auto,
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

    /// Select the protocol era, skipping the probe [`McpProtocolMode::Auto`]
    /// would otherwise make.
    pub fn protocol_mode(mut self, mode: McpProtocolMode) -> Self {
        self.protocol_mode = mode;
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
    /// Record the era the client settled on. Only a transport that has to
    /// vary its framing by era needs to act on this; HTTP negotiates for
    /// itself and ignores it.
    async fn adopt(&self, _negotiated: &Negotiated) {}
}

/// A `tools/list` response held for as long as the server said it stays
/// fresh — the `ttlMs` hint `2026-07-28` requires on cacheable results.
struct CachedTools {
    tools: Vec<ToolDefinition>,
    expires_at: Instant,
}

/// What a server reports from `server/discover`.
///
/// `serverInfo` is self-reported and unverified: display it, log it, but do
/// not make security decisions with it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct McpServerDiscovery {
    /// Protocol versions the server will accept.
    pub supported_versions: Vec<String>,
    /// The server's advertised capabilities, verbatim.
    pub capabilities: Value,
    /// The server's self-reported name and version, when it sent them.
    pub server_info: Option<Value>,
    /// Natural-language guidance on using this server, when offered.
    pub instructions: Option<String>,
}

/// Minimal MCP client: era negotiation, `tools/list`, `tools/call`, over
/// whichever transport the server is configured for.
pub struct McpClient {
    server_name: String,
    next_id: AtomicI64,
    transport: Box<dyn Transport>,
    tools_cache: Mutex<Option<CachedTools>>,
}

/// The stdio transport: newline-delimited JSON-RPC over a child process.
struct StdioTransport {
    server_name: String,
    pending: PendingMap,
    stdin: Mutex<ChildStdin>,
    /// Set once the client settles on a stateless era, after which every
    /// request carries `_meta`. `None` while probing, and for the lifetime
    /// of a server that turned out to want an `initialize` handshake.
    stateless_version: Mutex<Option<String>>,
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
            stateless_version: Mutex::new(None),
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
        let params = match self.stateless_version.lock().await.as_deref() {
            Some(version) => protocol::with_request_meta(params, version),
            None => params,
        };
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

    async fn adopt(&self, negotiated: &Negotiated) {
        if !negotiated.stateful {
            *self.stateless_version.lock().await = Some(negotiated.version.clone());
        }
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
    use protocol::{MCP_PROTOCOL_VERSION_STATEFUL, Negotiated};

    struct RawResponse {
        status: reqwest::StatusCode,
        content_type: String,
        session_id: Option<String>,
        body: String,
    }

    pub(super) struct HttpTransport {
        server_name: String,
        url: String,
        headers: Vec<(String, String)>,
        auth: Option<Arc<dyn McpAuthProvider>>,
        client: reqwest::Client,
        mode: McpProtocolMode,
        negotiated: Mutex<Option<Negotiated>>,
        negotiation_gate: Mutex<()>,
    }

    impl HttpTransport {
        pub(super) fn new(
            server_name: &str,
            url: &str,
            headers: &[(String, String)],
            auth: Option<Arc<dyn McpAuthProvider>>,
            mode: McpProtocolMode,
        ) -> Self {
            Self {
                server_name: server_name.to_string(),
                url: url.to_string(),
                headers: headers.to_vec(),
                auth,
                client: reqwest::Client::new(),
                mode,
                negotiated: Mutex::new(mode.initial()),
                negotiation_gate: Mutex::new(()),
            }
        }

        async fn send_raw(
            &self,
            payload: Value,
            negotiated: &Negotiated,
            method: &str,
            tool_name: Option<&str>,
        ) -> Result<RawResponse> {
            let mut builder = self
                .client
                .post(&self.url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream");
            for (key, value) in &self.headers {
                builder = builder.header(key, value);
            }
            for (key, value) in protocol::routable_headers(negotiated, method, tool_name) {
                builder = builder.header(key, value);
            }
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
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let session_id = response
                .headers()
                .get(protocol::HEADER_SESSION_ID)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let status = response.status();
            let body = response.text().await.map_err(|error| {
                Error::Mcp(format!("mcp `{}` read failed: {error}", self.server_name))
            })?;
            Ok(RawResponse {
                status,
                content_type,
                session_id,
                body,
            })
        }

        fn status_error(&self, response: &RawResponse) -> Error {
            let hint = match response.status.as_u16() {
                401 | 403 => " — check the McpAuthProvider for this server",
                _ => "",
            };
            Error::Mcp(format!(
                "mcp `{}` returned {}{hint}: {}",
                self.server_name,
                response.status,
                response.body.trim()
            ))
        }

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

        async fn handshake(&self, preferred_version: &str, initialize_id: i64) -> Result<Value> {
            let initial = Negotiated::stateful(preferred_version);
            let params = json!({
                "protocolVersion": preferred_version,
                "capabilities": {},
                "clientInfo": {
                    "name": "agentyk",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            });
            let response = self
                .send_raw(
                    json!({
                        "jsonrpc": "2.0",
                        "id": initialize_id,
                        "method": "initialize",
                        "params": params,
                    }),
                    &initial,
                    "initialize",
                    None,
                )
                .await?;
            if !response.status.is_success() {
                return Err(self.status_error(&response));
            }
            let envelope = self.extract(initialize_id, &response.content_type, &response.body)?;
            let version = envelope["result"]["protocolVersion"]
                .as_str()
                .unwrap_or(preferred_version)
                .to_string();
            let negotiated = Negotiated {
                version,
                stateful: true,
                session_id: response.session_id,
            };
            *self.negotiated.lock().await = Some(negotiated.clone());
            Ok(envelope)
        }

        async fn send_operation(
            &self,
            id: i64,
            method: &str,
            params: Value,
            negotiated: &Negotiated,
        ) -> Result<Value> {
            let tool_name = (method == "tools/call")
                .then(|| params["name"].as_str().map(str::to_string))
                .flatten();
            let response = self
                .send_raw(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": method,
                        "params": protocol::with_request_meta(params, &negotiated.version),
                    }),
                    negotiated,
                    method,
                    tool_name.as_deref(),
                )
                .await?;
            if !response.status.is_success() {
                return Err(self.status_error(&response));
            }
            self.extract(id, &response.content_type, &response.body)
        }

        /// Settle the server's era on the first operation, then send it.
        ///
        /// HTTP has status codes to read, so the spec's backward-compatibility
        /// probe is the real request rather than a `server/discover` round
        /// trip: optimistically speak the stateless protocol and inspect what
        /// comes back. A spec-allocated error code proves a modern server
        /// however the request failed, and is answered on its own terms;
        /// anything else that complains about sessions or versions is a
        /// server still waiting for `initialize`.
        async fn negotiate_and_send(&self, id: i64, method: &str, params: Value) -> Result<Value> {
            let _gate = self.negotiation_gate.lock().await;
            if let Some(negotiated) = self.negotiated.lock().await.clone() {
                return self.send_operation(id, method, params, &negotiated).await;
            }

            let stateless = Negotiated::stateless(protocol::MCP_PROTOCOL_VERSION_LATEST);
            let tool_name = (method == "tools/call")
                .then(|| params["name"].as_str().map(str::to_string))
                .flatten();
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": protocol::with_request_meta(params.clone(), &stateless.version),
            });
            let probe = self
                .send_raw(payload, &stateless, method, tool_name.as_deref())
                .await?;

            if let Some(error) = self.spec_error_in(&probe) {
                return self.retry_as_modern(id, method, params, error).await;
            }
            if protocol::looks_like_handshake_required(probe.status.as_u16(), &probe.body) {
                return self
                    .handshake_then_send(id, method, params, MCP_PROTOCOL_VERSION_STATEFUL)
                    .await;
            }
            if !probe.status.is_success() {
                return Err(self.status_error(&probe));
            }
            let envelope = self.extract(id, &probe.content_type, &probe.body)?;
            *self.negotiated.lock().await = Some(stateless);
            Ok(envelope)
        }

        /// A spec-allocated error in a probe's body, whatever its status.
        ///
        /// The body may be JSON or a one-message SSE stream, and on a 4xx it
        /// is often neither; none of that is worth an error here, because a
        /// body we cannot read simply is not a modern error.
        fn spec_error_in(&self, probe: &RawResponse) -> Option<protocol::SpecError> {
            let envelope = if probe.content_type.starts_with("text/event-stream") {
                probe
                    .body
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok())?
            } else {
                serde_json::from_str::<Value>(&probe.body).ok()?
            };
            protocol::spec_error(&envelope)
        }

        /// Answer a modern server that refused our opening request.
        async fn retry_as_modern(
            &self,
            id: i64,
            method: &str,
            params: Value,
            error: protocol::SpecError,
        ) -> Result<Value> {
            if error.code != protocol::ERROR_UNSUPPORTED_PROTOCOL_VERSION {
                return Err(Error::Mcp(format!(
                    "mcp `{}` rejected `{method}` with code {}: {}",
                    self.server_name, error.code, error.message
                )));
            }
            let negotiated =
                protocol::negotiated_from_supported(&error.supported).ok_or_else(|| {
                    Error::Mcp(format!(
                        "mcp `{}` supports no protocol version agentyk speaks (it offers {})",
                        self.server_name,
                        if error.supported.is_empty() {
                            "none".to_string()
                        } else {
                            error.supported.join(", ")
                        }
                    ))
                })?;
            if negotiated.stateful {
                let version = negotiated.version.clone();
                return self.handshake_then_send(id, method, params, &version).await;
            }
            let result = self.send_operation(id, method, params, &negotiated).await?;
            *self.negotiated.lock().await = Some(negotiated);
            Ok(result)
        }

        /// Fall back to the `initialize` handshake, then send the operation
        /// the probe was carrying.
        async fn handshake_then_send(
            &self,
            id: i64,
            method: &str,
            params: Value,
            version: &str,
        ) -> Result<Value> {
            self.handshake(version, 0).await?;
            let _ = self.notify("notifications/initialized", json!({})).await;
            let negotiated = self
                .negotiated
                .lock()
                .await
                .clone()
                .expect("handshake stores negotiation");
            self.send_operation(id, method, params, &negotiated).await
        }
    }

    #[async_trait]
    impl Transport for HttpTransport {
        async fn request(&self, id: i64, method: &str, params: Value) -> Result<Value> {
            if method == "initialize" {
                let preferred = params["protocolVersion"]
                    .as_str()
                    .unwrap_or(MCP_PROTOCOL_VERSION_STATEFUL);
                return self.handshake(preferred, id).await;
            }
            if self.mode == McpProtocolMode::Auto && self.negotiated.lock().await.is_none() {
                return self.negotiate_and_send(id, method, params).await;
            }
            let negotiated = self
                .negotiated
                .lock()
                .await
                .clone()
                .ok_or_else(|| Error::Mcp("HTTP protocol was not initialized".into()))?;
            self.send_operation(id, method, params, &negotiated).await
        }

        async fn notify(&self, method: &str, params: Value) -> Result<()> {
            let negotiated = self
                .negotiated
                .lock()
                .await
                .clone()
                .ok_or_else(|| Error::Mcp("HTTP protocol was not initialized".into()))?;
            let response = self
                .send_raw(
                    json!({
                        "jsonrpc": "2.0",
                        "method": method,
                        "params": params,
                    }),
                    &negotiated,
                    method,
                    None,
                )
                .await?;
            if response.status.is_success() {
                Ok(())
            } else {
                Err(self.status_error(&response))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn transport() -> HttpTransport {
            HttpTransport::new(
                "demo",
                "http://127.0.0.1:1/mcp",
                &[],
                None,
                McpProtocolMode::Auto,
            )
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
    /// Connect to the server.
    ///
    /// Stdio and pinned stateful HTTP modes initialize immediately. The
    /// `Auto` and `Latest` HTTP modes defer protocol selection until the
    /// first operation. A stdio server's child process is killed when the
    /// client is dropped.
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
            McpTransport::Http { url, headers } => Box::new(http::HttpTransport::new(
                &server.name,
                url,
                headers,
                auth,
                server.protocol_mode,
            )),
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
            tools_cache: Mutex::new(None),
        };

        let handshake_version = match &server.transport {
            McpTransport::Stdio { .. } => client.negotiate_stdio(server.protocol_mode).await?,
            McpTransport::Http { .. } => server.protocol_mode.handshake_version(),
        };
        if let Some(protocol_version) = handshake_version {
            client.handshake(protocol_version).await?;
        }
        Ok(client)
    }

    /// Decide which era a stdio server speaks, returning the version to
    /// handshake at — or `None` when the server is stateless and there is no
    /// handshake to do.
    ///
    /// stdio has no status codes to drive a fallback, so the spec's probe is
    /// `server/discover`, which every `2026-07-28` server must implement.
    /// Answering it (or refusing with a spec-allocated code) marks a modern
    /// server; an unknown-method error, an unreadable reply, or silence
    /// marks one that still wants `initialize`. The probe gets a short
    /// deadline of its own: a legacy server that simply ignores the method
    /// must not cost a full request timeout on every connect.
    async fn negotiate_stdio(&self, mode: McpProtocolMode) -> Result<Option<&'static str>> {
        if mode != McpProtocolMode::Auto {
            let negotiated = match mode {
                McpProtocolMode::Latest => Negotiated::stateless(MCP_PROTOCOL_VERSION_LATEST),
                _ => Negotiated::stateful(
                    mode.handshake_version()
                        .unwrap_or(protocol::MCP_PROTOCOL_VERSION_STATEFUL),
                ),
            };
            self.transport.adopt(&negotiated).await;
            return Ok(mode.handshake_version());
        }

        let probe = tokio::time::timeout(
            DISCOVER_PROBE_TIMEOUT,
            self.transport.request(
                self.next_id.fetch_add(1, Ordering::Relaxed),
                protocol::METHOD_DISCOVER,
                protocol::with_request_meta(json!({}), MCP_PROTOCOL_VERSION_LATEST),
            ),
        )
        .await;

        let negotiated = match &probe {
            Ok(Ok(envelope)) => match protocol::spec_error(envelope) {
                // A modern server that will not speak our opening version
                // still told us which ones it will.
                Some(error) => protocol::negotiated_from_supported(&error.supported),
                None if envelope.get("error").is_some() => None,
                // A `DiscoverResult` names the versions on offer; an older
                // reply we cannot read that way is not a modern server.
                None => protocol::negotiated_from_supported(
                    &discovery_from(&envelope["result"]).supported_versions,
                ),
            },
            _ => None,
        };
        let Some(negotiated) = negotiated else {
            tracing::debug!(
                target: "agentyk::mcp",
                server = %self.server_name,
                "stdio server did not answer server/discover; falling back to initialize"
            );
            let fallback = Negotiated::stateful(protocol::MCP_PROTOCOL_VERSION_STATEFUL);
            self.transport.adopt(&fallback).await;
            return Ok(Some(protocol::MCP_PROTOCOL_VERSION_STATEFUL));
        };

        self.transport.adopt(&negotiated).await;
        if !negotiated.stateful {
            return Ok(None);
        }
        // A modern-looking server that only offers stateful versions still
        // needs the handshake, at a version we know we share with it.
        Ok(protocol::KNOWN_STATEFUL_VERSIONS
            .into_iter()
            .find(|version| *version == negotiated.version))
    }

    /// Run the `initialize` handshake and confirm it.
    async fn handshake(&self, protocol_version: &str) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {
                    "name": "agentyk",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
        .await?;
        self.transport
            .notify("notifications/initialized", json!({}))
            .await
    }

    /// Ask the server to describe itself: supported protocol versions,
    /// capabilities, identity, and any usage guidance.
    ///
    /// Every `2026-07-28` server implements this; one from an earlier
    /// revision will refuse it as an unknown method.
    pub async fn discover(&self) -> Result<McpServerDiscovery> {
        let result = self.request(protocol::METHOD_DISCOVER, json!({})).await?;
        Ok(discovery_from(&result))
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
        let result = response.get("result").cloned().unwrap_or(Value::Null);
        check_result_type(&self.server_name, method, &result)?;
        Ok(result)
    }

    /// Ask the server what tools it offers.
    ///
    /// `2026-07-28` requires a `ttlMs` freshness hint on this result, so a
    /// repeat call within that window is answered from memory rather than
    /// over the wire. That matters more than it sounds: a capability
    /// resolves its tools on every turn, and a stable list keeps the
    /// model's prompt cache intact as well as saving the round trip. A
    /// server that sends no hint is never cached.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        if let Some(cached) = self.cached_tools().await {
            return Ok(cached);
        }
        let result = self.request("tools/list", json!({})).await?;
        let tools = parse_tool_list(&result);
        if let Some(ttl) = protocol::cache_ttl(&result) {
            *self.tools_cache.lock().await = Some(CachedTools {
                tools: tools.clone(),
                expires_at: Instant::now() + ttl,
            });
        }
        Ok(tools)
    }

    async fn cached_tools(&self) -> Option<Vec<ToolDefinition>> {
        let cache = self.tools_cache.lock().await;
        let cached = cache.as_ref()?;
        (cached.expires_at > Instant::now()).then(|| cached.tools.clone())
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

/// Reject any result we would otherwise misread.
///
/// From `2026-07-28` every result carries a `resultType`. `"complete"` is the
/// ordinary one; `"input_required"` means the server wants roots, sampling,
/// or elicitation answered before it can finish — the Multi Round-Trip
/// Request pattern, which we do not implement. Such a result has no
/// `content`, so accepting it would hand the model an empty tool output as
/// though the call had succeeded. An absent field means a server from an
/// earlier revision, which the spec says to read as `"complete"`.
pub(crate) fn check_result_type(server_name: &str, method: &str, result: &Value) -> Result<()> {
    let Some(result_type) = result.get("resultType").and_then(Value::as_str) else {
        return Ok(());
    };
    match result_type {
        "complete" => Ok(()),
        "input_required" => Err(Error::Mcp(format!(
            "mcp `{server_name}` `{method}` needs input mid-call (multi round-trip \
             requests), which agentyk does not support"
        ))),
        other => Err(Error::Mcp(format!(
            "mcp `{server_name}` `{method}` returned unknown resultType `{other}`"
        ))),
    }
}

pub(crate) fn discovery_from(result: &Value) -> McpServerDiscovery {
    McpServerDiscovery {
        supported_versions: result["supportedVersions"]
            .as_array()
            .map(|versions| {
                versions
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        capabilities: result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})),
        server_info: result["_meta"].get(protocol::SERVER_INFO_META_KEY).cloned(),
        instructions: result["instructions"].as_str().map(str::to_string),
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
    fn a_complete_or_absent_result_type_passes() {
        assert!(
            check_result_type("demo", "tools/call", &json!({"content": []})).is_ok(),
            "a pre-2026-07-28 server omits the field; the spec reads that as complete"
        );
        assert!(
            check_result_type("demo", "tools/call", &json!({"resultType": "complete"})).is_ok()
        );
    }

    #[test]
    fn an_input_required_result_is_an_error_not_an_empty_output() {
        let error = check_result_type(
            "demo",
            "tools/call",
            &json!({
                "resultType": "input_required",
                "inputRequests": [{"method": "elicitation/create"}],
            }),
        )
        .expect_err("MRTR is unsupported");
        assert!(error.to_string().contains("multi round-trip"), "{error}");
    }

    #[test]
    fn an_unknown_result_type_fails_loudly() {
        let error = check_result_type("demo", "tools/list", &json!({"resultType": "partial"}))
            .expect_err("unknown result types are not silently read as complete");
        assert!(error.to_string().contains("partial"), "{error}");
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
