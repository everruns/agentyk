//! OpenAI Chat Completions driver. Also serves any OpenAI-compatible
//! endpoint (OpenRouter, local runtimes, proxies) via `ModelSpec::base_url`.

use async_trait::async_trait;
use serde_json::{Value, json};

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, DriverId, Usage};
use agentyk_core::error::{Error, Result};
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiDriver {
    id: DriverId,
    client: reqwest::Client,
}

impl OpenAiDriver {
    pub fn new() -> Self {
        Self::with_id(DriverId::openai())
    }

    /// Register the same protocol under a different driver id (e.g.
    /// `"openrouter"`) so multiple OpenAI-compatible providers can coexist.
    pub fn with_id(id: DriverId) -> Self {
        Self {
            id,
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// OpenAI accepts `content` as either a plain string or an array of typed
/// parts. Send a plain string for the common text-only case (matches what
/// most OpenAI-compatible servers expect and keeps the wire payload small);
/// switch to the array form only when an image is present.
fn wire_content(content: &[ContentPart]) -> Value {
    if content
        .iter()
        .all(|part| matches!(part, ContentPart::Text(_)))
    {
        return Value::String(
            content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join(""),
        );
    }
    Value::Array(
        content
            .iter()
            .map(|part| match part {
                ContentPart::Text(text) => json!({"type": "text", "text": text.text}),
                ContentPart::Image(image) => {
                    let url = image.url.clone().unwrap_or_else(|| {
                        format!(
                            "data:{};base64,{}",
                            image.media_type.as_deref().unwrap_or("image/png"),
                            image.base64.as_deref().unwrap_or_default()
                        )
                    });
                    json!({"type": "image_url", "image_url": {"url": url}})
                }
            })
            .collect(),
    )
}

fn to_wire_message(message: &Message) -> Value {
    match message.role {
        Role::System => json!({"role": "system", "content": wire_content(&message.content)}),
        Role::User => json!({"role": "user", "content": wire_content(&message.content)}),
        Role::Assistant => {
            let mut wire = json!({"role": "assistant", "content": wire_content(&message.content)});
            if !message.tool_calls.is_empty() {
                wire["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    "arguments": c.arguments.to_string(),
                                },
                            })
                        })
                        .collect(),
                );
            }
            wire
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id,
            "content": wire_content(&message.content),
        }),
    }
}

pub(crate) fn parse_tool_call_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    }
}

#[async_trait]
impl ChatDriver for OpenAiDriver {
    fn id(&self) -> DriverId {
        self.id.clone()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let base_url = request
            .model
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let mut messages = Vec::new();
        if let Some(system) = &request.system_prompt {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.extend(request.messages.iter().map(to_wire_message));

        let mut body = json!({
            "model": request.model.model,
            "messages": messages,
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            },
                        })
                    })
                    .collect(),
            );
        }

        let mut http = self
            .client
            .post(format!(
                "{}/chat/completions",
                base_url.trim_end_matches('/')
            ))
            .json(&body);
        if let Some(key) = &request.model.api_key {
            http = http.bearer_auth(key);
        }

        let response = http
            .send()
            .await
            .map_err(|e| Error::Driver(format!("openai request failed: {e}")))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|e| Error::Driver(format!("openai response decode failed: {e}")))?;
        if !status.is_success() {
            return Err(Error::Driver(format!("openai http {status}: {payload}")));
        }

        let choice = &payload["choices"][0]["message"];
        let content = choice["content"].as_str().unwrap_or_default().to_string();
        let tool_calls = choice["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| ToolCall {
                        id: c["id"].as_str().unwrap_or_default().to_string(),
                        name: c["function"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        arguments: parse_tool_call_arguments(&c["function"]["arguments"]),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let usage = Usage {
            input_tokens: payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: payload["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        };

        Ok(ChatResponse {
            message: Message::assistant_with_calls(content, tool_calls),
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_parse_from_string_or_object() {
        assert_eq!(
            parse_tool_call_arguments(&json!("{\"a\":1}")),
            json!({"a": 1})
        );
        assert_eq!(parse_tool_call_arguments(&json!({"a": 1})), json!({"a": 1}));
        assert_eq!(parse_tool_call_arguments(&json!("not json")), json!({}));
    }

    #[test]
    fn text_only_content_serializes_as_plain_string() {
        let message = Message::user("hello");
        let wire = to_wire_message(&message);
        assert_eq!(wire["content"], "hello");
    }

    #[test]
    fn image_content_serializes_as_content_part_array() {
        let message = Message::user_multimodal(vec![
            ContentPart::text("what is this?"),
            ContentPart::image_url("https://example.com/cat.png"),
        ]);
        let wire = to_wire_message(&message);
        assert_eq!(wire["content"][0]["type"], "text");
        assert_eq!(wire["content"][1]["type"], "image_url");
        assert_eq!(
            wire["content"][1]["image_url"]["url"],
            "https://example.com/cat.png"
        );
    }

    #[test]
    fn assistant_tool_calls_serialize_arguments_as_string() {
        let message = Message::assistant_with_calls(
            "",
            vec![ToolCall {
                id: "call_0".into(),
                name: "add".into(),
                arguments: json!({"a": 1}),
            }],
        );
        let wire = to_wire_message(&message);
        assert_eq!(wire["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
    }
}
