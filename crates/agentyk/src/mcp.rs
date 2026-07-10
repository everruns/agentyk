//! MCP — Model Context Protocol support.
//!
//! [`McpCapability`] connects to an MCP server over stdio (newline-delimited
//! JSON-RPC 2.0), discovers its tools, and exposes them to the model as
//! ordinary [`Tool`]s. Attach it like any other capability:
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
//! The connection is lazy: the server process is spawned on the first turn
//! that resolves tools.

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

/// Configuration for a stdio MCP server.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServer {
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// Minimal stdio MCP client: initialize handshake, `tools/list`,
/// `tools/call`.
pub struct McpClient {
    server_name: String,
    next_id: AtomicI64,
    pending: PendingMap,
    stdin: Mutex<ChildStdin>,
    _child: Child,
}

impl McpClient {
    pub async fn connect(server: &McpServer) -> Result<Self> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &server.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| {
            Error::Mcp(format!(
                "failed to spawn mcp server `{}` ({}): {e}",
                server.name, server.command
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

        let client = Self {
            server_name: server.name.clone(),
            next_id: AtomicI64::new(1),
            pending,
            stdin: Mutex::new(stdin),
            _child: child,
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
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
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

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);

        self.write_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| {
                Error::Mcp(format!(
                    "mcp `{}` timed out on `{method}`",
                    self.server_name
                ))
            })?
            .map_err(|_| Error::Mcp(format!("mcp `{}` closed", self.server_name)))?;

        if let Some(error) = response.get("error") {
            return Err(Error::Mcp(format!(
                "mcp `{}` `{method}` failed: {error}",
                self.server_name
            )));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(parse_tool_list(&result))
    }

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
                    Some(ToolDefinition {
                        name: tool["name"].as_str()?.to_string(),
                        description: tool["description"].as_str().unwrap_or_default().to_string(),
                        parameters: if tool["inputSchema"].is_object() {
                            tool["inputSchema"].clone()
                        } else {
                            json!({"type": "object"})
                        },
                    })
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
    ToolOutput {
        content,
        is_error: result["isError"].as_bool().unwrap_or(false),
    }
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
    client: OnceCell<Arc<McpClient>>,
}

impl McpCapability {
    pub fn new(server: McpServer) -> Self {
        Self {
            id: format!("mcp:{}", server.name),
            server,
            client: OnceCell::new(),
        }
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
            .get_or_try_init(|| async { McpClient::connect(&self.server).await.map(Arc::new) })
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
