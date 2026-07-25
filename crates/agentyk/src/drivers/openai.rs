//! OpenAI Chat Completions driver. Also serves any OpenAI-compatible
//! endpoint (OpenRouter, local runtimes, proxies) via `ModelSpec::base_url`.
//!
//! Wire mapping only — sending, status/transport classification, and SSE
//! framing live in the crate-internal `drivers::http` layer.

use async_trait::async_trait;
use serde_json::{Value, json};

use agentyk_core::driver::{
    ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, ModelSpec, Usage,
};
use agentyk_core::error::Result;
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};

use super::http::{self, HttpProvider, StreamAccumulator};

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

    /// Supply your own client — timeouts, proxies, and connection pooling are
    /// its business, not the driver's.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
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

/// Arguments arrive as a JSON *string* on the non-streaming path and as
/// fragments of one on the streaming path; both land here.
pub(crate) fn parse_tool_call_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    }
}

fn tool_calls_from_choice(choice: &Value) -> Vec<ToolCall> {
    choice["tool_calls"]
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
        .unwrap_or_default()
}

fn usage_from(payload: &Value) -> Usage {
    Usage::new(
        payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        payload["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    )
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a Chat Completions streaming response: `delta.content`
/// fragments concatenate directly; `delta.tool_calls[].function.arguments`
/// fragments are JSON-fragment text keyed by `index` and only parsed once
/// complete, at [`StreamAccumulator::finish`].
#[derive(Default)]
pub(crate) struct OpenAiStream {
    content: String,
    tool_calls: Vec<PartialToolCall>,
    usage: Usage,
}

impl StreamAccumulator for OpenAiStream {
    fn apply(&mut self, payload: &Value) -> Option<String> {
        // Usage rides the same stream as the deltas (with
        // `stream_options.include_usage`), typically on a final chunk whose
        // `choices` array is empty.
        if payload.get("usage").is_some_and(|u| !u.is_null()) {
            self.usage = usage_from(payload);
        }

        let delta = &payload["choices"][0]["delta"];
        let mut text_delta = None;
        if let Some(content) = delta["content"].as_str()
            && !content.is_empty()
        {
            self.content.push_str(content);
            text_delta = Some(content.to_string());
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(PartialToolCall::default());
                }
                let entry = &mut self.tool_calls[index];
                if let Some(id) = call["id"].as_str() {
                    entry.id.push_str(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    entry.arguments.push_str(args);
                }
            }
        }
        text_delta
    }

    fn text(&self) -> &str {
        &self.content
    }

    fn finish(self) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| ToolCall {
                id: c.id,
                name: c.name,
                arguments: parse_tool_call_arguments(&Value::String(c.arguments)),
            })
            .collect();
        let message = if tool_calls.is_empty() {
            Message::assistant(self.content)
        } else {
            Message::assistant_with_calls(self.content, tool_calls)
        };
        ChatResponse::new(message, self.usage)
    }
}

impl HttpProvider for OpenAiDriver {
    type Accumulator = OpenAiStream;

    fn label(&self) -> &str {
        "openai"
    }

    fn default_base_url(&self) -> &str {
        DEFAULT_BASE_URL
    }

    fn endpoint(&self) -> &str {
        "/chat/completions"
    }

    /// A key is optional: local runtimes and proxies routinely need none.
    fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder> {
        Ok(match &model.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        })
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut messages = Vec::new();
        if let Some(system) = &request.system_prompt {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.extend(request.messages.iter().map(to_wire_message));

        let mut body = json!({
            "model": request.model.model,
            "messages": messages,
        });
        if let Some(effort) = request
            .model
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.as_deref())
        {
            body["reasoning_effort"] = json!(effort);
        }
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
        body
    }

    fn enable_streaming(&self, body: &mut Value) {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
    }

    fn parse_response(&self, payload: &Value) -> ChatResponse {
        let choice = &payload["choices"][0]["message"];
        let content = choice["content"].as_str().unwrap_or_default().to_string();
        ChatResponse::new(
            Message::assistant_with_calls(content, tool_calls_from_choice(choice)),
            usage_from(payload),
        )
    }
}

#[async_trait]
impl ChatDriver for OpenAiDriver {
    fn id(&self) -> DriverId {
        self.id.clone()
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
    fn reasoning_effort_is_forwarded_when_set() {
        let body = OpenAiDriver::new().build_body(&request(
            ModelSpec::openai("gpt-5.5").reasoning_effort("high"),
        ));
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn reasoning_effort_omitted_when_unset() {
        let body = OpenAiDriver::new().build_body(&request(ModelSpec::openai("gpt-5.5")));
        assert!(body.get("reasoning_effort").is_none());
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

    #[test]
    fn parse_response_reads_content_calls_and_usage() {
        let payload = json!({
            "choices": [{"message": {
                "content": "done",
                "tool_calls": [{"id": "c1", "function": {"name": "add", "arguments": "{\"a\":1}"}}],
            }}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 9},
        });
        let response = OpenAiDriver::new().parse_response(&payload);
        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
        assert_eq!(response.usage, Usage::new(5, 9));
    }

    #[tokio::test]
    async fn streaming_reports_deltas_and_returns_the_final_message() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenAiStream>(
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
                // Split mid-line, to prove reassembly through the real loop.
                "data: {\"choices\":[{\"delta\":{\"conte",
                "nt\":\"lo\"}}]}\n",
                "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n",
                "data: [DONE]\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.deltas, vec!["Hel", "lo"]);
        assert_eq!(sink.accumulated, vec!["Hel", "Hello"]);
        assert_eq!(response.message.text(), "Hello");
        assert_eq!(response.usage, Usage::new(3, 2));
    }

    #[tokio::test]
    async fn streaming_assembles_tool_call_fragments_by_index() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenAiStream>(
            &[
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"function\":{\"name\":\"add\",\"arguments\":\"\"}}]}}]}\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert!(sink.deltas.is_empty(), "tool JSON is not answer text");
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].id, "call_0");
        assert_eq!(response.message.tool_calls[0].name, "add");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
    }
}
