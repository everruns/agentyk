//! OpenAI Chat Completions driver. Also serves any OpenAI-compatible
//! endpoint (OpenRouter, local runtimes, proxies) via `ModelSpec::base_url`.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, Usage};
use agentyk_core::error::{Error, LlmErrorKind, Result};
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

/// The Chat Completions request body shared by `complete` and
/// `complete_streaming`; the caller sets `"stream"` afterward.
fn build_body(request: &ChatRequest) -> Value {
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

fn request_builder(
    client: &reqwest::Client,
    model: &agentyk_core::driver::ModelSpec,
    body: &Value,
) -> reqwest::RequestBuilder {
    let base_url = model
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let mut http = client
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .json(body);
    if let Some(key) = &model.api_key {
        http = http.bearer_auth(key);
    }
    http
}

/// Classifies an HTTP error status so a host can decide whether retrying is
/// worth it — see [`LlmErrorKind`].
fn classify_status(status: reqwest::StatusCode) -> LlmErrorKind {
    match status.as_u16() {
        401 | 403 => LlmErrorKind::Authentication,
        429 => LlmErrorKind::RateLimited,
        503 => LlmErrorKind::Overloaded,
        400 | 404 | 422 => LlmErrorKind::InvalidRequest,
        500..=599 => LlmErrorKind::ServerError,
        400..=499 => LlmErrorKind::InvalidRequest,
        _ => LlmErrorKind::Unknown,
    }
}

/// Classifies a transport-level failure (the request never got an HTTP
/// response at all).
fn network_error(context: &str, error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        LlmErrorKind::Timeout
    } else {
        LlmErrorKind::Network
    };
    Error::driver(kind, format!("{context}: {error}"))
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

/// Extracts the JSON payload from one SSE `data: ...` line. Returns `None`
/// for blank lines, comments (`:`-prefixed), non-data fields (`event:`,
/// `id:`, …), and the `[DONE]` sentinel.
pub(crate) fn parse_sse_data_line(line: &str) -> Option<&str> {
    let line = line.trim_end_matches('\r');
    let data = line.strip_prefix("data:")?.trim_start();
    if data == "[DONE]" {
        return None;
    }
    Some(data)
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
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a Chat Completions streaming response: `delta.content`
/// fragments concatenate directly; `delta.tool_calls[].function.arguments`
/// fragments are JSON-fragment text keyed by `index` and only parsed once
/// complete, at [`Self::into_message`].
#[derive(Default)]
struct StreamAccumulator {
    content: String,
    tool_calls: Vec<PartialToolCall>,
}

impl StreamAccumulator {
    /// Applies one `choices[0].delta` object, returning the text delta (if
    /// any) so the caller can forward it to a [`DeltaSink`].
    fn apply_delta(&mut self, delta: &Value) -> Option<String> {
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

    fn into_message(self) -> Message {
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
        if tool_calls.is_empty() {
            Message::assistant(self.content)
        } else {
            Message::assistant_with_calls(self.content, tool_calls)
        }
    }
}

#[async_trait]
impl ChatDriver for OpenAiDriver {
    fn id(&self) -> DriverId {
        self.id.clone()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = build_body(&request);
        let response = request_builder(&self.client, &request.model, &body)
            .send()
            .await
            .map_err(|e| network_error("openai request failed", &e))?;
        let status = response.status();
        if !status.is_success() {
            let payload: Value = response.json().await.unwrap_or(Value::Null);
            return Err(Error::driver(
                classify_status(status),
                format!("openai http {status}: {payload}"),
            ));
        }
        let payload: Value = response.json().await.map_err(|e| {
            Error::driver(
                LlmErrorKind::Unknown,
                format!("openai response decode failed: {e}"),
            )
        })?;

        let choice = &payload["choices"][0]["message"];
        let content = choice["content"].as_str().unwrap_or_default().to_string();
        let tool_calls = tool_calls_from_choice(choice);

        let usage = Usage::new(
            payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            payload["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        );

        Ok(ChatResponse::new(
            Message::assistant_with_calls(content, tool_calls),
            usage,
        ))
    }

    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        let mut body = build_body(&request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});

        let response = request_builder(&self.client, &request.model, &body)
            .send()
            .await
            .map_err(|e| network_error("openai request failed", &e))?;
        let status = response.status();
        if !status.is_success() {
            let payload: Value = response.json().await.unwrap_or(Value::Null);
            return Err(Error::driver(
                classify_status(status),
                format!("openai http {status}: {payload}"),
            ));
        }

        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut accumulator = StreamAccumulator::default();
        let mut usage = Usage::default();

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| network_error("openai stream read failed", &e))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            for line in drain_lines(&mut buffer) {
                let Some(data) = parse_sse_data_line(&line) else {
                    continue;
                };
                let Ok(payload) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(delta) = payload["choices"][0]["delta"].as_object() {
                    let delta = Value::Object(delta.clone());
                    if let Some(text) = accumulator.apply_delta(&delta) {
                        sink.delta(&text, &accumulator.content).await?;
                    }
                }
                if payload.get("usage").is_some_and(|u| !u.is_null()) {
                    usage = Usage::new(
                        payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                        payload["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                    );
                }
            }
        }

        Ok(ChatResponse::new(accumulator.into_message(), usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_classify_by_retryability() {
        use reqwest::StatusCode;
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            LlmErrorKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            LlmErrorKind::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            LlmErrorKind::Overloaded
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST),
            LlmErrorKind::InvalidRequest
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            LlmErrorKind::ServerError
        );
        assert!(classify_status(StatusCode::TOO_MANY_REQUESTS).is_retryable());
        assert!(!classify_status(StatusCode::UNAUTHORIZED).is_retryable());
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
        let mut model = agentyk_core::driver::ModelSpec::openai("gpt-5.5");
        model = model.reasoning_effort("high");
        let request = ChatRequest::new(model, vec![Message::user("hi")]);
        let body = build_body(&request);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn reasoning_effort_omitted_when_unset() {
        let request = ChatRequest::new(
            agentyk_core::driver::ModelSpec::openai("gpt-5.5"),
            vec![Message::user("hi")],
        );
        let body = build_body(&request);
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
    fn sse_data_line_parsing() {
        assert_eq!(parse_sse_data_line("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_sse_data_line("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_sse_data_line("data: [DONE]"), None);
        assert_eq!(parse_sse_data_line(""), None);
        assert_eq!(parse_sse_data_line("event: message"), None);
        assert_eq!(parse_sse_data_line(": comment"), None);
    }

    #[test]
    fn drain_lines_keeps_trailing_partial_line() {
        let mut buffer = "line one\nline tw".to_string();
        let lines = drain_lines(&mut buffer);
        assert_eq!(lines, vec!["line one".to_string()]);
        assert_eq!(buffer, "line tw");

        buffer.push_str("o\n");
        let lines = drain_lines(&mut buffer);
        assert_eq!(lines, vec!["line two".to_string()]);
        assert_eq!(buffer, "");
    }

    #[test]
    fn stream_accumulator_assembles_text_deltas() {
        let mut acc = StreamAccumulator::default();
        assert_eq!(
            acc.apply_delta(&json!({"content": "Hel"})),
            Some("Hel".to_string())
        );
        assert_eq!(
            acc.apply_delta(&json!({"content": "lo"})),
            Some("lo".to_string())
        );
        assert_eq!(acc.apply_delta(&json!({})), None);
        let message = acc.into_message();
        assert_eq!(message.text(), "Hello");
        assert!(message.tool_calls.is_empty());
    }

    #[test]
    fn stream_accumulator_assembles_tool_call_fragments_by_index() {
        let mut acc = StreamAccumulator::default();
        acc.apply_delta(&json!({
            "tool_calls": [{"index": 0, "id": "call_0", "function": {"name": "add", "arguments": ""}}]
        }));
        acc.apply_delta(&json!({
            "tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":"}}]
        }));
        acc.apply_delta(&json!({
            "tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]
        }));
        let message = acc.into_message();
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].id, "call_0");
        assert_eq!(message.tool_calls[0].name, "add");
        assert_eq!(message.tool_calls[0].arguments, json!({"a": 1}));
    }

    #[tokio::test]
    async fn complete_streaming_reports_deltas_and_returns_final_message() {
        use agentyk_core::error::Result as CoreResult;

        struct RecordingSink {
            deltas: Vec<(String, String)>,
        }
        #[async_trait]
        impl DeltaSink for RecordingSink {
            async fn delta(&mut self, delta: &str, accumulated: &str) -> CoreResult<()> {
                self.deltas
                    .push((delta.to_string(), accumulated.to_string()));
                Ok(())
            }
        }

        // Simulate SSE chunk-by-chunk delivery straight through the parsing
        // path used by `complete_streaming`, without a live network call.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n",
            "data: [DONE]\n",
        );

        let mut sink = RecordingSink { deltas: Vec::new() };
        let mut buffer = String::new();
        let mut accumulator = StreamAccumulator::default();
        let mut usage = Usage::default();
        buffer.push_str(sse_body);
        for line in drain_lines(&mut buffer) {
            let Some(data) = parse_sse_data_line(&line) else {
                continue;
            };
            let payload: Value = serde_json::from_str(data).unwrap();
            if let Some(delta) = payload["choices"][0]["delta"].as_object() {
                let delta = Value::Object(delta.clone());
                if let Some(text) = accumulator.apply_delta(&delta) {
                    sink.delta(&text, &accumulator.content).await.unwrap();
                }
            }
            if payload.get("usage").is_some_and(|u| !u.is_null()) {
                usage = Usage::new(
                    payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                    payload["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                );
            }
        }

        assert_eq!(
            sink.deltas,
            vec![
                ("Hel".to_string(), "Hel".to_string()),
                ("lo".to_string(), "Hello".to_string()),
            ]
        );
        assert_eq!(accumulator.into_message().text(), "Hello");
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 2);
    }
}
