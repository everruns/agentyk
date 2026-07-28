//! Anthropic Messages API driver.
//!
//! Wire mapping only — sending, status/transport classification, and SSE
//! framing live in the crate-internal `drivers::http` layer.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use agentyk_core::driver::{
    AuthHeaderProvider, ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, ModelSpec,
    Usage,
};
use agentyk_core::error::{Error, LlmErrorKind, Result};
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};

use super::http::{self, HttpProvider, StreamAccumulator, decode};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Speaks the Anthropic Messages protocol.
///
/// Requires an api key on the [`ModelSpec`]. Streams real incremental deltas.
pub struct AnthropicDriver {
    client: reqwest::Client,
    max_tokens: u64,
    prompt_caching: bool,
    auth: Option<Arc<dyn AuthHeaderProvider>>,
}

impl AnthropicDriver {
    /// A driver on a default HTTP client.
    pub fn new() -> Self {
        Self::with_client(super::http::client())
    }

    /// Supply your own client — timeouts, proxies, and connection pooling are
    /// its business, not the driver's.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            max_tokens: DEFAULT_MAX_TOKENS,
            prompt_caching: true,
            auth: None,
        }
    }

    /// Cap the response length (default 8192). Raised automatically when a
    /// thinking budget would otherwise exceed it, which the API rejects.
    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Turn prompt caching on or off (default: **on**).
    ///
    /// An agent re-sends its whole transcript every turn, so by the tenth
    /// turn most of every request is text the provider has already seen.
    /// Caching marks that prefix so it is billed and processed at a fraction
    /// of the cost. The driver places up to four `cache_control` breakpoints
    /// per request: the tool array, the system prompt, and the last two
    /// messages — the pair being what makes caching *incremental* across a
    /// growing transcript.
    ///
    /// Reasons to turn it off: a proxy that rejects the field, or a workload
    /// of one-shot prompts with nothing in common, where the cache writes
    /// cost more than they save.
    pub fn prompt_caching(mut self, enabled: bool) -> Self {
        self.prompt_caching = enabled;
        self
    }

    /// Resolve authentication immediately before every request.
    ///
    /// This overrides [`ModelSpec::api_key`]. The provider must return the
    /// complete provider-specific header, such as `x-api-key` or an OAuth
    /// `Authorization` bearer value.
    pub fn with_auth_provider(mut self, provider: impl AuthHeaderProvider + 'static) -> Self {
        self.auth = Some(Arc::new(provider));
        self
    }

    /// Attach a shared request-time authentication provider.
    pub fn with_auth_provider_arc(mut self, provider: Arc<dyn AuthHeaderProvider>) -> Self {
        self.auth = Some(provider);
        self
    }

    /// Place `cache_control` breakpoints on a built request body.
    ///
    /// Anthropic caches the request *prefix* up to each breakpoint and allows
    /// at most four, so where they go decides what is reusable:
    ///
    /// 1. **Tools** — the last tool definition, which covers the whole tool
    ///    array. Identical on every turn of a session.
    /// 2. **System prompt** — after the tools in prefix order, and equally
    ///    static.
    /// 3. **The previous message** and 4. **the last message** — the growing
    ///    part. Two breakpoints, not one, is what makes caching *incremental*:
    ///    a turn reads the cache written one turn ago while writing a new one
    ///    that covers everything up to now. With a single moving breakpoint
    ///    each turn would write a cache nothing ever reads.
    ///
    /// A prefix shorter than the model's minimum cacheable length is simply
    /// not cached — the API ignores the marker rather than failing, so no
    /// length heuristic is needed here.
    fn cache_breakpoints(body: &mut Value) {
        let marker = json!({"type": "ephemeral"});

        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
            && let Some(last) = tools.last_mut()
        {
            last["cache_control"] = marker.clone();
        }

        // `system` is built as a plain string; caching needs the block form.
        if let Some(system) = body.get("system").and_then(Value::as_str) {
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": marker.clone(),
            }]);
        }

        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        let count = messages.len();
        // The last message, and the one before it when there is one.
        let breakpoints: Vec<usize> = match count {
            0 => Vec::new(),
            1 => vec![0],
            _ => vec![count - 2, count - 1],
        };
        for index in breakpoints {
            if let Some(blocks) = messages[index]
                .get_mut("content")
                .and_then(Value::as_array_mut)
                && let Some(last) = blocks.last_mut()
            {
                last["cache_control"] = marker.clone();
            }
        }
    }
}

impl Default for AnthropicDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert content parts to Anthropic content blocks (`text` / `image`).
fn content_blocks(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({"type": "text", "text": text.text}),
            ContentPart::Image(image) => {
                let source = if let Some(url) = &image.url {
                    json!({"type": "url", "url": url})
                } else {
                    json!({
                        "type": "base64",
                        "media_type": image.media_type.as_deref().unwrap_or("image/png"),
                        "data": image.base64.as_deref().unwrap_or_default(),
                    })
                };
                json!({"type": "image", "source": source})
            }
        })
        .collect()
}

/// Convert the flat message list to Anthropic's user/assistant alternation:
/// tool results become `tool_result` blocks inside a user message, and
/// assistant tool calls become `tool_use` blocks.
pub(crate) fn to_wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut wire: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |wire: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            wire.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    };

    for message in messages {
        match message.role {
            // A tool_result's content may itself be blocks, so images a tool
            // returned reach the model in place — no relay message needed.
            Role::Tool => pending_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": message.tool_call_id,
                "content": content_blocks(&message.content),
            })),
            Role::User => {
                flush_tool_results(&mut wire, &mut pending_tool_results);
                wire.push(json!({"role": "user", "content": content_blocks(&message.content)}));
            }
            Role::Assistant => {
                flush_tool_results(&mut wire, &mut pending_tool_results);
                let mut blocks: Vec<Value> = Vec::new();
                // Extended thinking must be replayed first, with its
                // signature, or the API rejects the assistant turn.
                if let (Some(thinking), Some(signature)) =
                    (&message.thinking, &message.thinking_signature)
                {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                }
                blocks.extend(content_blocks(&message.content));
                for call in &message.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                wire.push(json!({"role": "assistant", "content": blocks}));
            }
            // System text travels via the top-level `system` parameter.
            Role::System => {}
        }
    }
    flush_tool_results(&mut wire, &mut pending_tool_results);
    wire
}

/// One block of a Messages response's `content` array.
///
/// Unknown block *types* deserialize to [`ContentBlock::Other`] rather than
/// failing: Anthropic adds block types (`redacted_thinking`,
/// `server_tool_use`, …) and a response carrying one is still perfectly
/// usable. A known block whose *fields* changed does fail, which is the
/// distinction worth having — that is the case that would otherwise hand the
/// model an empty message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    /// Input tokens written to the prompt cache this request.
    #[serde(default)]
    cache_creation_input_tokens: u64,
    /// Input tokens served from the prompt cache this request.
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl From<WireUsage> for Usage {
    /// Cached tokens are counted as input tokens.
    ///
    /// Anthropic reports them in separate fields (they are billed at
    /// different rates), and `input_tokens` excludes them. Summing keeps
    /// `Usage.input_tokens` meaning "tokens this request sent", so enabling
    /// caching does not make a session look like it suddenly stopped sending
    /// context. Cost, which does differ per field, is a host concern — a host
    /// that needs the split reads it from its own accounting, not from a
    /// provider-neutral `Usage`.
    fn from(usage: WireUsage) -> Self {
        Usage::new(
            usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens,
            usage.output_tokens,
        )
    }
}

/// A non-streaming Messages response.
///
/// `content` is required: its absence means the shape we depend on has
/// changed, and reporting that beats returning a blank answer. `usage` is
/// defaulted, because losing token counts is a reporting gap, not a wrong
/// answer.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: WireUsage,
}

/// Fold content blocks into an assistant [`Message`]: concatenated text,
/// `tool_use` blocks, and any extended-thinking block (kept on
/// `thinking`/`thinking_signature` so it round-trips to the provider).
fn message_from_blocks(blocks: Vec<ContentBlock>) -> Message {
    let mut text = String::new();
    let mut thinking: Option<(String, Option<String>)> = None;
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: part } => text.push_str(&part),
            ContentBlock::Thinking {
                thinking: part,
                signature,
            } => thinking = Some((part, signature)),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                id,
                name,
                arguments: input,
            }),
            ContentBlock::Other => {}
        }
    }
    let message = Message::assistant_with_calls(text, tool_calls);
    match thinking {
        Some((thinking, signature)) => message.with_thinking(thinking, signature),
        None => message,
    }
}

#[derive(Default)]
struct PartialBlock {
    is_tool_use: bool,
    tool_id: String,
    tool_name: String,
    json_fragments: String,
}

/// One streaming event. Unknown event types and unknown delta types
/// deserialize to `Other` — Anthropic adds both — while a known one that
/// changed shape fails loudly.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    MessageStart {
        message: StreamMessageStart,
    },
    ContentBlockStart {
        #[serde(default)]
        index: u64,
        content_block: StreamBlockStart,
    },
    ContentBlockDelta {
        #[serde(default)]
        index: u64,
        delta: StreamDelta,
    },
    MessageDelta {
        #[serde(default)]
        usage: WireUsage,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct StreamMessageStart {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamBlockStart {
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    #[serde(other)]
    Other,
}

/// Accumulates a Messages API streaming response across
/// `message_start` / `content_block_start` / `content_block_delta` /
/// `message_delta` events.
#[derive(Default)]
pub(crate) struct AnthropicStream {
    text: String,
    /// Extended-thinking text + signature, accumulated separately from the
    /// answer text and never forwarded to the delta sink (it's reasoning, not
    /// the response).
    thinking: String,
    thinking_signature: Option<String>,
    blocks: HashMap<u64, PartialBlock>,
    usage: Usage,
}

impl StreamAccumulator for AnthropicStream {
    fn apply(&mut self, data: &str) -> Result<Option<String>> {
        let event: StreamEvent = decode("anthropic", data)?;
        Ok(match event {
            StreamEvent::MessageStart { message } => {
                self.usage.input_tokens = message.usage.input_tokens;
                None
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block: StreamBlockStart::ToolUse { id, name },
            } => {
                self.blocks.insert(
                    index,
                    PartialBlock {
                        is_tool_use: true,
                        tool_id: id,
                        tool_name: name,
                        json_fragments: String::new(),
                    },
                );
                None
            }
            StreamEvent::ContentBlockDelta { index, delta } => match delta {
                StreamDelta::TextDelta { text } => {
                    self.text.push_str(&text);
                    Some(text)
                }
                StreamDelta::InputJsonDelta { partial_json } => {
                    self.blocks
                        .entry(index)
                        .or_default()
                        .json_fragments
                        .push_str(&partial_json);
                    None
                }
                // Reasoning: accumulate, but don't surface as answer text.
                StreamDelta::ThinkingDelta { thinking } => {
                    self.thinking.push_str(&thinking);
                    None
                }
                StreamDelta::SignatureDelta { signature } => {
                    self.thinking_signature = Some(signature);
                    None
                }
                StreamDelta::Other => None,
            },
            StreamEvent::MessageDelta { usage } => {
                if usage.output_tokens > 0 {
                    self.usage.output_tokens = usage.output_tokens;
                }
                None
            }
            StreamEvent::ContentBlockStart { .. } | StreamEvent::Other => None,
        })
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn finish(self) -> ChatResponse {
        let mut indices: Vec<u64> = self.blocks.keys().copied().collect();
        indices.sort_unstable();
        let tool_calls: Vec<ToolCall> = indices
            .into_iter()
            .filter_map(|index| self.blocks.get(&index))
            .filter(|block| block.is_tool_use)
            .map(|block| ToolCall {
                id: block.tool_id.clone(),
                name: block.tool_name.clone(),
                arguments: if block.json_fragments.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&block.json_fragments).unwrap_or(json!({}))
                },
            })
            .collect();
        let message = if tool_calls.is_empty() {
            Message::assistant(self.text)
        } else {
            Message::assistant_with_calls(self.text, tool_calls)
        };
        let message = if self.thinking.is_empty() {
            message
        } else {
            message.with_thinking(self.thinking, self.thinking_signature)
        };
        ChatResponse::new(message, self.usage)
    }
}

#[async_trait]
impl HttpProvider for AnthropicDriver {
    type Accumulator = AnthropicStream;

    fn label(&self) -> &str {
        "anthropic"
    }

    fn default_base_url(&self) -> &str {
        DEFAULT_BASE_URL
    }

    fn endpoint(&self) -> &str {
        "/v1/messages"
    }

    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder> {
        let builder = if let Some(provider) = &self.auth {
            http::apply_auth_header(builder, provider.auth_header().await?)?
        } else {
            let api_key = model.api_key.clone().ok_or_else(|| {
                Error::driver(
                    LlmErrorKind::Authentication,
                    "anthropic driver requires an api key",
                )
            })?;
            builder.header("x-api-key", api_key)
        };
        Ok(builder.header("anthropic-version", API_VERSION))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        // Extended thinking is enabled per-request with a token budget; the
        // API requires max_tokens to exceed the budget, so grow it if needed.
        let thinking_budget = request
            .model
            .reasoning
            .as_ref()
            .and_then(|r| r.budget_tokens);
        let effective_max_tokens = match thinking_budget {
            Some(budget) if u64::from(budget) >= self.max_tokens => {
                u64::from(budget) + self.max_tokens
            }
            _ => self.max_tokens,
        };

        let mut body = json!({
            "model": request.model.model,
            "max_tokens": effective_max_tokens,
            "messages": to_wire_messages(&request.messages),
        });
        if let Some(budget) = thinking_budget {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        if let Some(system) = &request.system_prompt {
            body["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            );
        }
        if self.prompt_caching {
            Self::cache_breakpoints(&mut body);
        }
        body
    }

    fn enable_streaming(&self, body: &mut Value) {
        body["stream"] = json!(true);
    }

    fn parse_response(&self, body: &str) -> Result<ChatResponse> {
        let MessagesResponse { content, usage } = decode(self.label(), body)?;
        Ok(ChatResponse::new(
            message_from_blocks(content),
            usage.into(),
        ))
    }

    /// 529 is Anthropic's own "overloaded" code, on top of the generic 503.
    fn classify_status(&self, status: reqwest::StatusCode) -> LlmErrorKind {
        match status.as_u16() {
            529 => LlmErrorKind::Overloaded,
            _ => http::classify_status(status),
        }
    }
}

#[async_trait]
impl ChatDriver for AnthropicDriver {
    fn id(&self) -> DriverId {
        DriverId::anthropic()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        http::complete(self, &self.client, request).await
    }

    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        http::complete_streaming(self, &self.client, request, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::http::{RecordingSink, drive_stream};

    fn request(model: ModelSpec) -> ChatRequest {
        ChatRequest::new(model, vec![Message::user("hi")])
    }

    fn tool(name: &str) -> agentyk_core::tool::ToolDefinition {
        agentyk_core::tool::ToolDefinition::new(name, "does a thing", json!({"type": "object"}))
    }

    #[test]
    fn prompt_caching_marks_tools_system_and_the_last_two_messages() {
        let driver = AnthropicDriver::new();
        let mut request = ChatRequest::new(
            ModelSpec::anthropic("claude-x"),
            vec![
                Message::user("first"),
                Message::assistant("second"),
                Message::user("third"),
            ],
        );
        request.system_prompt = Some("be terse".into());
        request.tools = vec![tool("read_file"), tool("write_file")];

        let body = driver.build_body(&request);

        // The system prompt becomes a block array so it can carry a marker.
        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // Only the last tool is marked; the breakpoint covers the whole array.
        assert!(body["tools"][0]["cache_control"].is_null());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        // The last two messages, so each turn reads the previous turn's cache.
        let messages = body["messages"].as_array().expect("messages");
        assert!(messages[0]["content"][0]["cache_control"].is_null());
        assert_eq!(
            messages[1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // Four is the API ceiling; never exceed it.
        let markers = serde_json::to_string(&body)
            .unwrap()
            .matches("cache_control")
            .count();
        assert!(markers <= 4, "{markers} breakpoints exceeds the API limit");
    }

    #[test]
    fn a_single_message_request_still_gets_one_conversation_breakpoint() {
        let driver = AnthropicDriver::new();
        let body = driver.build_body(&request(ModelSpec::anthropic("claude-x")));
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn caching_can_be_turned_off_for_proxies_that_reject_it() {
        let driver = AnthropicDriver::new().prompt_caching(false);
        let mut request = request(ModelSpec::anthropic("claude-x"));
        request.system_prompt = Some("be terse".into());
        request.tools = vec![tool("read_file")];

        let body = driver.build_body(&request);
        assert_eq!(body["system"], json!("be terse"), "plain string, as before");
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("cache_control")
        );
    }

    #[test]
    fn cached_tokens_are_counted_as_input_tokens() {
        let usage: Usage = serde_json::from_value::<WireUsage>(json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 1000,
        }))
        .unwrap()
        .into();
        assert_eq!(usage.input_tokens, 1110);
        assert_eq!(usage.output_tokens, 5);
    }

    #[test]
    fn anthropics_own_overloaded_code_is_classified_as_retryable() {
        use reqwest::StatusCode;
        let driver = AnthropicDriver::new();
        let overloaded = StatusCode::from_u16(529).unwrap();
        assert_eq!(driver.classify_status(overloaded), LlmErrorKind::Overloaded);
        assert!(driver.classify_status(overloaded).is_retryable());
        // Everything else falls through to the shared classification.
        assert_eq!(
            driver.classify_status(StatusCode::UNAUTHORIZED),
            LlmErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn missing_api_key_is_an_authentication_error() {
        let driver = AnthropicDriver::new();
        let builder = reqwest::Client::new().post("https://example.invalid");
        let error = driver
            .authorize(builder, &ModelSpec::anthropic("claude-x"))
            .await
            .unwrap_err();
        assert!(!error.is_retryable());
        assert!(matches!(
            error,
            Error::Driver {
                kind: LlmErrorKind::Authentication,
                ..
            }
        ));
    }

    #[test]
    fn image_content_becomes_an_image_block() {
        let messages = vec![Message::user_multimodal(vec![
            ContentPart::text("what is this?"),
            ContentPart::image_base64("Zm9v", "image/png"),
        ])];
        let wire = to_wire_messages(&messages);
        assert_eq!(wire[0]["content"][0]["type"], "text");
        assert_eq!(wire[0]["content"][1]["type"], "image");
        assert_eq!(wire[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(wire[0]["content"][1]["source"]["data"], "Zm9v");
    }

    #[test]
    fn tool_results_merge_into_one_user_message() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant_with_calls(
                "",
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "one".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "two".into(),
                        arguments: json!({}),
                    },
                ],
            ),
            Message::tool_result("a", "1"),
            Message::tool_result("b", "2"),
        ];
        let wire = to_wire_messages(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[2]["role"], "user");
        assert_eq!(wire[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(wire[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn thinking_budget_enables_thinking_and_grows_max_tokens() {
        let driver = AnthropicDriver::new();
        let body = driver.build_body(&request(
            ModelSpec::anthropic("claude-x").thinking_budget(12000),
        ));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 12000);
        // max_tokens must exceed the budget.
        assert!(body["max_tokens"].as_u64().unwrap() > 12000);
    }

    #[test]
    fn no_thinking_param_without_a_budget() {
        let driver = AnthropicDriver::new();
        let body = driver.build_body(&request(ModelSpec::anthropic("claude-x")));
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn assistant_thinking_replays_as_a_leading_block() {
        let messages = vec![
            Message::assistant("the answer is 4").with_thinking("2+2…", Some("sig-xyz".into())),
        ];
        let wire = to_wire_messages(&messages);
        // Thinking block first, with its signature, then the text.
        assert_eq!(wire[0]["content"][0]["type"], "thinking");
        assert_eq!(wire[0]["content"][0]["thinking"], "2+2…");
        assert_eq!(wire[0]["content"][0]["signature"], "sig-xyz");
        assert_eq!(wire[0]["content"][1]["type"], "text");
    }

    #[test]
    fn assistant_without_signature_omits_the_thinking_block() {
        // A thinking block can't be replayed without its signature.
        let messages = vec![Message::assistant("hi").with_thinking("…", None)];
        let wire = to_wire_messages(&messages);
        assert_eq!(wire[0]["content"][0]["type"], "text");
    }

    #[test]
    fn parse_response_extracts_thinking_text_calls_and_usage() {
        let payload = json!({
            "content": [
                {"type": "thinking", "thinking": "let me see", "signature": "sig-1"},
                {"type": "text", "text": "done"},
                {"type": "tool_use", "id": "t1", "name": "add", "input": {"a": 1}},
            ],
            "usage": {"input_tokens": 11, "output_tokens": 3},
        })
        .to_string();
        let response = AnthropicDriver::new().parse_response(&payload).unwrap();
        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.thinking.as_deref(), Some("let me see"));
        assert_eq!(
            response.message.thinking_signature.as_deref(),
            Some("sig-1")
        );
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.usage, Usage::new(11, 3));
    }

    /// An unrecognized block type is expected — Anthropic adds them — and
    /// must not cost us the rest of the response.
    #[test]
    fn parse_response_ignores_unknown_block_types() {
        let payload = json!({
            "content": [
                {"type": "server_tool_use", "id": "s1", "name": "web_search"},
                {"type": "text", "text": "answer"},
            ],
        })
        .to_string();
        let response = AnthropicDriver::new().parse_response(&payload).unwrap();
        assert_eq!(response.message.text(), "answer");
    }

    /// The case this typing exists for: a shape we depend on changing now
    /// names the problem instead of handing the model a blank message.
    #[test]
    fn parse_response_reports_a_changed_shape_instead_of_returning_nothing() {
        // `content` renamed by a future API version.
        let payload = json!({"blocks": [{"type": "text", "text": "hi"}]}).to_string();
        let error = AnthropicDriver::new().parse_response(&payload).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("did not match the expected shape"),
            "{message}"
        );
        assert!(message.contains("content"), "{message}");
        // Not retryable: replaying the same request cannot fix a shape change.
        assert!(!error.is_retryable());
    }

    /// A text block missing its `text` is a real decode failure, not an
    /// unknown-type case, and is reported as one.
    #[test]
    fn parse_response_reports_a_known_block_with_missing_fields() {
        let payload = json!({"content": [{"type": "text"}]}).to_string();
        let error = AnthropicDriver::new().parse_response(&payload).unwrap_err();
        assert!(error.to_string().contains("text"), "{error}");
    }

    #[tokio::test]
    async fn streaming_ignores_unknown_event_types_but_reports_broken_known_ones() {
        let mut sink = RecordingSink::default();
        // An unknown event type passes through harmlessly...
        let response = drive_stream::<AnthropicStream>(
            &["data: {\"type\":\"ping\"}\n", "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n"],
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(response.message.text(), "ok");

        // ...while a known event whose shape changed is surfaced.
        let mut sink = RecordingSink::default();
        let error = drive_stream::<AnthropicStream>(
            &["data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\"}}\n"],
            &mut sink,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("text"), "{error}");
    }

    #[tokio::test]
    async fn streaming_reports_text_deltas_in_order_and_collects_usage() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<AnthropicStream>(
            &[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n",
                // Split mid-line, to prove reassembly through the real loop.
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"del",
                "ta\":{\"type\":\"text_delta\",\"text\":\"!\"}}\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n",
                "data: {\"type\":\"message_stop\"}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.deltas, vec!["Hi", "!"]);
        assert_eq!(sink.accumulated, vec!["Hi", "Hi!"]);
        assert_eq!(response.message.text(), "Hi!");
        assert_eq!(response.usage, Usage::new(7, 2));
        assert!(response.message.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn streaming_assembles_tool_use_from_json_fragments() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<AnthropicStream>(
            &[
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"add\",\"input\":{}}}\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert!(sink.deltas.is_empty(), "tool JSON is not answer text");
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].id, "toolu_1");
        assert_eq!(response.message.tool_calls[0].name, "add");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
    }

    #[tokio::test]
    async fn streaming_collects_thinking_without_polluting_the_answer() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<AnthropicStream>(
            &[
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-stream\"}}\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        // Reasoning never reaches the sink...
        assert_eq!(sink.deltas, vec!["answer"]);
        // ...but does round-trip on the message.
        assert_eq!(response.message.text(), "answer");
        assert_eq!(response.message.thinking.as_deref(), Some("hmm"));
        assert_eq!(
            response.message.thinking_signature.as_deref(),
            Some("sig-stream")
        );
    }
}
