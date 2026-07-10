//! Anthropic Messages API driver.

use async_trait::async_trait;
use serde_json::{Value, json};

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, DriverId, Usage};
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

#[async_trait]
impl ChatDriver for AnthropicDriver {
    fn id(&self) -> DriverId {
        DriverId::anthropic()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let base_url = request
            .model
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let mut body = json!({
            "model": request.model.model,
            "max_tokens": self.max_tokens,
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

        let api_key = request
            .model
            .api_key
            .clone()
            .ok_or_else(|| Error::Driver("anthropic driver requires an api key".into()))?;

        let response = self
            .client
            .post(format!("{}/v1/messages", base_url.trim_end_matches('/')))
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
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
}
