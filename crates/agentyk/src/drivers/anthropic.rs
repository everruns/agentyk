//! Anthropic Messages API driver.
//!
//! Wire mapping only — sending, status/transport classification, and SSE
//! framing live in the crate-internal `drivers::http` layer.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Value, json};

use agentyk_core::driver::{
    ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, ModelSpec, Usage,
};
use agentyk_core::error::{Error, LlmErrorKind, Result};
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};

use super::http::{self, HttpProvider, StreamAccumulator};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;

pub struct AnthropicDriver {
    client: reqwest::Client,
    max_tokens: u64,
}

impl AnthropicDriver {
    pub fn new() -> Self {
        Self::with_client(reqwest::Client::new())
    }

    /// Supply your own client — timeouts, proxies, and connection pooling are
    /// its business, not the driver's.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
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
            Role::Tool => pending_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": message.tool_call_id,
                "content": message.text(),
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

/// Parse a non-streaming Messages response body into an assistant [`Message`]:
/// concatenated `text`, `tool_use` blocks, and any extended-thinking block
/// (kept on `Message::thinking`/`thinking_signature` so it round-trips).
pub(crate) fn parse_message(payload: &Value) -> Message {
    let mut text = String::new();
    let mut thinking: Option<String> = None;
    let mut signature: Option<String> = None;
    let mut tool_calls = Vec::new();
    if let Some(blocks) = payload["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                Some("thinking") => {
                    thinking = Some(block["thinking"].as_str().unwrap_or_default().to_string());
                    signature = block["signature"].as_str().map(str::to_string);
                }
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    arguments: block["input"].clone(),
                }),
                _ => {}
            }
        }
    }
    let message = Message::assistant_with_calls(text, tool_calls);
    match thinking {
        Some(thinking) => message.with_thinking(thinking, signature),
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
    fn apply(&mut self, payload: &Value) -> Option<String> {
        match payload["type"].as_str() {
            Some("message_start") => {
                self.usage.input_tokens = payload["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                None
            }
            Some("content_block_start") => {
                let index = payload["index"].as_u64().unwrap_or(0);
                let block = payload["content_block"].clone();
                if block["type"].as_str() == Some("tool_use") {
                    self.blocks.insert(
                        index,
                        PartialBlock {
                            is_tool_use: true,
                            tool_id: block["id"].as_str().unwrap_or_default().to_string(),
                            tool_name: block["name"].as_str().unwrap_or_default().to_string(),
                            json_fragments: String::new(),
                        },
                    );
                }
                None
            }
            Some("content_block_delta") => {
                let index = payload["index"].as_u64().unwrap_or(0);
                match payload["delta"]["type"].as_str() {
                    Some("text_delta") => {
                        let text = payload["delta"]["text"].as_str().unwrap_or_default();
                        self.text.push_str(text);
                        Some(text.to_string())
                    }
                    Some("input_json_delta") => {
                        let fragment = payload["delta"]["partial_json"]
                            .as_str()
                            .unwrap_or_default();
                        self.blocks
                            .entry(index)
                            .or_default()
                            .json_fragments
                            .push_str(fragment);
                        None
                    }
                    // Reasoning: accumulate, but don't surface as answer text.
                    Some("thinking_delta") => {
                        self.thinking
                            .push_str(payload["delta"]["thinking"].as_str().unwrap_or_default());
                        None
                    }
                    Some("signature_delta") => {
                        if let Some(sig) = payload["delta"]["signature"].as_str() {
                            self.thinking_signature = Some(sig.to_string());
                        }
                        None
                    }
                    _ => None,
                }
            }
            Some("message_delta") => {
                if let Some(tokens) = payload["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = tokens;
                }
                None
            }
            _ => None,
        }
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

    fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder> {
        let api_key = model.api_key.clone().ok_or_else(|| {
            Error::driver(
                LlmErrorKind::Authentication,
                "anthropic driver requires an api key",
            )
        })?;
        Ok(builder
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION))
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
        body
    }

    fn enable_streaming(&self, body: &mut Value) {
        body["stream"] = json!(true);
    }

    fn parse_response(&self, payload: &Value) -> ChatResponse {
        ChatResponse::new(
            parse_message(payload),
            Usage::new(
                payload["usage"]["input_tokens"].as_u64().unwrap_or(0),
                payload["usage"]["output_tokens"].as_u64().unwrap_or(0),
            ),
        )
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

    #[test]
    fn missing_api_key_is_an_authentication_error() {
        let driver = AnthropicDriver::new();
        let builder = reqwest::Client::new().post("https://example.invalid");
        let error = driver
            .authorize(builder, &ModelSpec::anthropic("claude-x"))
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
        });
        let response = AnthropicDriver::new().parse_response(&payload);
        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.thinking.as_deref(), Some("let me see"));
        assert_eq!(
            response.message.thinking_signature.as_deref(),
            Some("sig-1")
        );
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.usage, Usage::new(11, 3));
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
