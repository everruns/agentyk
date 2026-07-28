//! OpenAI Chat Completions driver. Also serves any OpenAI-compatible
//! endpoint (OpenRouter, local runtimes, proxies) via `ModelSpec::base_url`.
//!
//! Wire mapping only — sending, status/transport classification, and SSE
//! framing live in the crate-internal `drivers::http` layer.

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

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Speaks the OpenAI Chat Completions protocol.
///
/// Also serves any compatible endpoint — OpenRouter, local runtimes, proxies
/// — via [`ModelSpec::base_url`]. An api key is optional, since local
/// runtimes routinely need none. Streams real incremental deltas.
pub struct OpenAiDriver {
    id: DriverId,
    client: reqwest::Client,
    auth: Option<Arc<dyn AuthHeaderProvider>>,
}

impl OpenAiDriver {
    /// A driver registered as `"openai"`, on a default HTTP client.
    pub fn new() -> Self {
        Self::with_id(DriverId::openai())
    }

    /// Register the same protocol under a different driver id (e.g.
    /// `"openrouter"`) so multiple OpenAI-compatible providers can coexist.
    pub fn with_id(id: DriverId) -> Self {
        Self {
            id,
            client: super::http::client(),
            auth: None,
        }
    }

    /// Supply your own client — timeouts, proxies, and connection pooling are
    /// its business, not the driver's.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Resolve authentication immediately before every request.
    ///
    /// This overrides [`ModelSpec::api_key`] and is the seam for OAuth token
    /// refresh, workload identity, and other short-lived credentials.
    pub fn with_auth_provider(mut self, provider: impl AuthHeaderProvider + 'static) -> Self {
        self.auth = Some(Arc::new(provider));
        self
    }

    /// Attach a shared request-time authentication provider.
    pub fn with_auth_provider_arc(mut self, provider: Arc<dyn AuthHeaderProvider>) -> Self {
        self.auth = Some(provider);
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

/// Expand one message into the wire messages it needs.
///
/// Almost always exactly one. The exception is a tool result carrying images:
/// the Chat Completions protocol accepts only text in a `tool` message, so the
/// images follow as a user message rather than being dropped. That is a
/// deliberate, visible workaround — the model still sees them, and the
/// transcript says where they came from. Anthropic needs no such trick (see
/// its driver), which is exactly why `ToolOutput::parts` is documented as
/// best-effort per protocol.
fn to_wire_messages(message: &Message) -> Vec<Value> {
    let mut wire = vec![to_wire_message(message)];
    if message.role == Role::Tool {
        let images: Vec<ContentPart> = message
            .content
            .iter()
            .filter(|part| matches!(part, ContentPart::Image(_)))
            .cloned()
            .collect();
        if !images.is_empty() {
            let mut content = vec![ContentPart::text(format!(
                "Images returned by tool call {}:",
                message.tool_call_id.as_deref().unwrap_or("(unknown)")
            ))];
            content.extend(images);
            wire.push(json!({"role": "user", "content": wire_content(&content)}));
        }
    }
    wire
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
        // Text only: images in a tool result travel as a following user
        // message — see `to_wire_messages`.
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id,
            "content": message.text(),
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

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Usage::new(usage.prompt_tokens, usage.completion_tokens)
    }
}

#[derive(Debug, Default, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: String,
    /// A JSON *string*, not an object — whole on the non-streaming path,
    /// fragmented on the streaming one.
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: WireFunction,
}

impl From<WireToolCall> for ToolCall {
    fn from(call: WireToolCall) -> Self {
        ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: parse_tool_call_arguments(&Value::String(call.function.arguments)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    /// `null` when the model only asked for tools.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

/// A non-streaming Chat Completions response.
///
/// `choices` is required, and an empty one is an error rather than an empty
/// answer: a completion with nowhere to read the reply from is a failure the
/// caller should see, not a blank turn.
#[derive(Debug, Deserialize)]
struct ChatCompletion {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// One streaming chunk. Every field is optional because a chunk carries
/// either deltas or the trailing usage report, never necessarily both.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    /// Which call in the batch this fragment belongs to.
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
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
    fn apply(&mut self, data: &str) -> Result<Option<String>> {
        let chunk: StreamChunk = decode("openai", data)?;

        // Usage rides the same stream as the deltas (with
        // `stream_options.include_usage`), typically on a final chunk whose
        // `choices` array is empty.
        if let Some(usage) = chunk.usage {
            self.usage = usage.into();
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(None);
        };

        let mut text_delta = None;
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            self.content.push_str(&content);
            text_delta = Some(content);
        }
        for call in choice.delta.tool_calls {
            while self.tool_calls.len() <= call.index {
                self.tool_calls.push(PartialToolCall::default());
            }
            let entry = &mut self.tool_calls[call.index];
            if let Some(id) = call.id {
                entry.id.push_str(&id);
            }
            if let Some(function) = call.function {
                entry.name.push_str(&function.name);
                entry.arguments.push_str(&function.arguments);
            }
        }
        Ok(text_delta)
    }

    /// `[DONE]` closes an OpenAI stream and is not JSON.
    fn is_terminator(data: &str) -> bool {
        data == "[DONE]"
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

#[async_trait]
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
    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder> {
        if let Some(provider) = &self.auth {
            return http::apply_auth_header(builder, provider.auth_header().await?);
        }
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
        messages.extend(request.messages.iter().flat_map(to_wire_messages));

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

    fn parse_response(&self, body: &str) -> Result<ChatResponse> {
        let response: ChatCompletion = decode(self.label(), body)?;
        let usage = Usage::from(response.usage);
        let choice = response.choices.into_iter().next().ok_or_else(|| {
            Error::driver(
                LlmErrorKind::Unknown,
                "openai returned a completion with no choices",
            )
        })?;
        let message = Message::assistant_with_calls(
            choice.message.content.unwrap_or_default(),
            choice
                .message
                .tool_calls
                .into_iter()
                .map(Into::into)
                .collect(),
        );
        Ok(ChatResponse::new(message, usage))
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
        let response = OpenAiDriver::new()
            .parse_response(&payload.to_string())
            .unwrap();
        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
        assert_eq!(response.usage, Usage::new(5, 9));
    }

    /// A completion with nowhere to read the reply from is a failure the
    /// caller should see, not a blank turn.
    #[test]
    fn parse_response_reports_an_empty_choices_array() {
        let payload = json!({"choices": []}).to_string();
        let error = OpenAiDriver::new().parse_response(&payload).unwrap_err();
        assert!(error.to_string().contains("no choices"), "{error}");
    }

    #[test]
    fn parse_response_reports_a_changed_shape_instead_of_returning_nothing() {
        let payload = json!({"completions": [{"message": {"content": "hi"}}]}).to_string();
        let error = OpenAiDriver::new().parse_response(&payload).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("did not match the expected shape"),
            "{message}"
        );
        assert!(message.contains("choices"), "{message}");
        assert!(!error.is_retryable());
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
