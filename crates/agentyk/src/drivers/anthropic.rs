//! Anthropic Messages API driver.

use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, Usage};
use agentyk_core::error::{Error, Result};
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;

pub struct AnthropicDriver {
    client: reqwest::Client,
    max_tokens: u64,
}

impl AnthropicDriver {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
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
                let mut blocks = content_blocks(&message.content);
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

/// The Messages API request body shared by `complete` and
/// `complete_streaming`; the caller sets `"stream"` afterward.
fn build_body(request: &ChatRequest, max_tokens: u64) -> Value {
    let mut body = json!({
        "model": request.model.model,
        "max_tokens": max_tokens,
        "messages": to_wire_messages(&request.messages),
    });
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

fn request_builder(
    client: &reqwest::Client,
    model: &agentyk_core::driver::ModelSpec,
    body: &Value,
) -> Result<reqwest::RequestBuilder> {
    let base_url = model
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key = model
        .api_key
        .clone()
        .ok_or_else(|| Error::Driver("anthropic driver requires an api key".into()))?;
    Ok(client
        .post(format!("{}/v1/messages", base_url.trim_end_matches('/')))
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .json(body))
}

/// Extracts the JSON payload from one SSE `data: ...` line. Anthropic also
/// sends `event: ...` lines, but each payload's own `"type"` field is
/// sufficient to interpret it, so non-`data:` lines are simply skipped.
pub(crate) fn parse_sse_data_line(line: &str) -> Option<&str> {
    line.trim_end_matches('\r')
        .strip_prefix("data:")
        .map(str::trim_start)
}

/// Drains complete `\n`-terminated lines from `buffer`, leaving any trailing
/// partial line for the next chunk.
fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buffer.find('\n') {
        lines.push(buffer[..pos].to_string());
        buffer.drain(..=pos);
    }
    lines
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
struct StreamAccumulator {
    text: String,
    blocks: HashMap<u64, PartialBlock>,
    usage: Usage,
}

impl StreamAccumulator {
    /// Applies one decoded SSE event payload, returning the text delta (if
    /// any) so the caller can forward it to a [`DeltaSink`].
    fn apply_event(&mut self, payload: &Value) -> Option<String> {
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

    fn into_message(self) -> Message {
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
        if tool_calls.is_empty() {
            Message::assistant(self.text)
        } else {
            Message::assistant_with_calls(self.text, tool_calls)
        }
    }
}

#[async_trait]
impl ChatDriver for AnthropicDriver {
    fn id(&self) -> DriverId {
        DriverId::anthropic()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = build_body(&request, self.max_tokens);
        let response = request_builder(&self.client, &request.model, &body)?
            .send()
            .await
            .map_err(|e| Error::Driver(format!("anthropic request failed: {e}")))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|e| Error::Driver(format!("anthropic response decode failed: {e}")))?;
        if !status.is_success() {
            return Err(Error::Driver(format!("anthropic http {status}: {payload}")));
        }

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(blocks) = payload["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                    Some("tool_use") => tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        arguments: block["input"].clone(),
                    }),
                    _ => {}
                }
            }
        }

        let usage = Usage {
            input_tokens: payload["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: payload["usage"]["output_tokens"].as_u64().unwrap_or(0),
        };

        Ok(ChatResponse {
            message: Message::assistant_with_calls(text, tool_calls),
            usage,
        })
    }

    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        let mut body = build_body(&request, self.max_tokens);
        body["stream"] = json!(true);

        let response = request_builder(&self.client, &request.model, &body)?
            .send()
            .await
            .map_err(|e| Error::Driver(format!("anthropic request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let payload: Value = response.json().await.unwrap_or(Value::Null);
            return Err(Error::Driver(format!("anthropic http {status}: {payload}")));
        }

        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut accumulator = StreamAccumulator::default();

        while let Some(chunk) = byte_stream.next().await {
            let chunk =
                chunk.map_err(|e| Error::Driver(format!("anthropic stream read failed: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            for line in drain_lines(&mut buffer) {
                let Some(data) = parse_sse_data_line(&line) else {
                    continue;
                };
                let Ok(payload) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(text) = accumulator.apply_event(&payload) {
                    sink.delta(&text, &accumulator.text).await?;
                }
            }
        }

        let usage = accumulator.usage;
        Ok(ChatResponse {
            message: accumulator.into_message(),
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sse_data_line_parsing() {
        assert_eq!(parse_sse_data_line("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_sse_data_line("event: message_start"), None);
        assert_eq!(parse_sse_data_line(""), None);
    }

    #[test]
    fn stream_accumulator_assembles_text_deltas_and_usage() {
        let mut acc = StreamAccumulator::default();
        acc.apply_event(
            &json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
        );
        assert_eq!(
            acc.apply_event(&json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "Hel"}
            })),
            Some("Hel".to_string())
        );
        assert_eq!(
            acc.apply_event(&json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "lo"}
            })),
            Some("lo".to_string())
        );
        acc.apply_event(&json!({"type": "message_delta", "usage": {"output_tokens": 5}}));

        assert_eq!(acc.usage.input_tokens, 10);
        assert_eq!(acc.usage.output_tokens, 5);
        let message = acc.into_message();
        assert_eq!(message.text(), "Hello");
        assert!(message.tool_calls.is_empty());
    }

    #[test]
    fn stream_accumulator_assembles_tool_use_from_json_deltas() {
        let mut acc = StreamAccumulator::default();
        acc.apply_event(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {}}
        }));
        acc.apply_event(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}
        }));
        acc.apply_event(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "1}"}
        }));
        let message = acc.into_message();
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].id, "toolu_1");
        assert_eq!(message.tool_calls[0].name, "add");
        assert_eq!(message.tool_calls[0].arguments, json!({"a": 1}));
    }

    #[tokio::test]
    async fn complete_streaming_reports_deltas_in_order() {
        use agentyk_core::error::Result as CoreResult;

        struct RecordingSink {
            deltas: Vec<String>,
        }
        #[async_trait]
        impl DeltaSink for RecordingSink {
            async fn delta(&mut self, delta: &str, _accumulated: &str) -> CoreResult<()> {
                self.deltas.push(delta.to_string());
                Ok(())
            }
        }

        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"!\"}}\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
        );

        let mut sink = RecordingSink { deltas: Vec::new() };
        let mut buffer = String::new();
        let mut accumulator = StreamAccumulator::default();
        buffer.push_str(sse_body);
        for line in drain_lines(&mut buffer) {
            let Some(data) = parse_sse_data_line(&line) else {
                continue;
            };
            let payload: Value = serde_json::from_str(data).unwrap();
            if let Some(text) = accumulator.apply_event(&payload) {
                sink.delta(&text, &accumulator.text).await.unwrap();
            }
        }

        assert_eq!(sink.deltas, vec!["Hi".to_string(), "!".to_string()]);
        assert_eq!(accumulator.usage.input_tokens, 7);
        assert_eq!(accumulator.usage.output_tokens, 2);
        assert_eq!(accumulator.into_message().text(), "Hi!");
    }
}
