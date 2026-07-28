//! Open Responses protocol driver.
//!
//! Implements the vendor-neutral Open Responses request and streaming event
//! shapes used by OpenAI, OpenRouter, and compatible endpoints. Authentication
//! is resolved per request through [`AuthHeaderProvider`] when configured.

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

/// Speaks the vendor-neutral Open Responses protocol.
///
/// The default endpoint is OpenAI's `/v1/responses`, but
/// [`ModelSpec::base_url`] can point at any compatible implementation. API
/// keys are optional for local endpoints. Use
/// [`OpenResponsesDriver::with_auth_provider`] for refreshable OAuth tokens.
pub struct OpenResponsesDriver {
    id: DriverId,
    label: &'static str,
    default_base_url: &'static str,
    require_auth: bool,
    client: reqwest::Client,
    auth: Option<Arc<dyn AuthHeaderProvider>>,
}

impl OpenResponsesDriver {
    /// A driver registered as `"openresponses"` on a default HTTP client.
    pub fn new() -> Self {
        Self::for_provider(
            DriverId::openresponses(),
            "openresponses",
            DEFAULT_BASE_URL,
            false,
        )
    }

    pub(crate) fn for_provider(
        id: DriverId,
        label: &'static str,
        default_base_url: &'static str,
        require_auth: bool,
    ) -> Self {
        Self {
            id,
            label,
            default_base_url,
            require_auth,
            client: super::http::client(),
            auth: None,
        }
    }

    /// Supply your own client for timeouts, proxies, and connection pooling.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Resolve authentication immediately before every request.
    ///
    /// This overrides [`ModelSpec::api_key`]. The provider may refresh and
    /// persist OAuth credentials before returning the next header.
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

impl Default for OpenResponsesDriver {
    fn default() -> Self {
        Self::new()
    }
}

fn image_url(part: &agentyk_core::message::ImageContentPart) -> String {
    part.url.clone().unwrap_or_else(|| {
        format!(
            "data:{};base64,{}",
            part.media_type.as_deref().unwrap_or("image/png"),
            part.base64.as_deref().unwrap_or_default()
        )
    })
}

fn message_content(message: &Message) -> Value {
    if message.is_text_only() {
        return Value::String(message.text());
    }

    Value::Array(
        message
            .content
            .iter()
            .map(|part| match part {
                ContentPart::Text(text) => {
                    let kind = if message.role == Role::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    json!({"type": kind, "text": text.text})
                }
                ContentPart::Image(image) => {
                    json!({"type": "input_image", "image_url": image_url(image)})
                }
            })
            .collect(),
    )
}

fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();
    let mut reasoning_index = 0usize;

    for message in messages {
        match message.role {
            Role::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.text(),
                }));
                let images: Vec<_> = message
                    .content
                    .iter()
                    .filter(|part| matches!(part, ContentPart::Image(_)))
                    .cloned()
                    .collect();
                if !images.is_empty() {
                    let relay = Message::user_multimodal(images);
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": message_content(&relay),
                    }));
                }
            }
            Role::Assistant => {
                if let Some(encrypted_content) = &message.thinking_signature {
                    reasoning_index += 1;
                    let mut reasoning = json!({
                        "type": "reasoning",
                        "id": format!("rs_{reasoning_index:08x}"),
                        "encrypted_content": encrypted_content,
                    });
                    if let Some(thinking) = &message.thinking {
                        reasoning["summary"] = json!([{"type": "summary_text", "text": thinking}]);
                    }
                    input.push(reasoning);
                }
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": message_content(message),
                    }));
                }
                input.extend(message.tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    })
                }));
            }
            Role::System | Role::User => {
                let role = if message.role == Role::System {
                    "developer"
                } else {
                    "user"
                };
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": message_content(message),
                }));
            }
        }
    }

    input
}

fn request_body(request: &ChatRequest) -> Value {
    let mut body = json!({
        "model": request.model.model,
        "input": input_items(&request.messages),
        "store": false,
    });
    if let Some(instructions) = &request.system_prompt {
        body["instructions"] = json!(instructions);
    }
    if let Some(effort) = request
        .model
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.as_deref())
    {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Usage::new(usage.input_tokens, usage.output_tokens)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputContent {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ReasoningSummary {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<OutputContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        id: String,
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        encrypted_content: Option<String>,
        #[serde(default)]
        summary: Vec<ReasoningSummary>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<OutputItem>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Default)]
struct ResponseParts {
    text: String,
    tool_calls: Vec<ToolCall>,
    thinking: String,
    thinking_signature: Option<String>,
    usage: Usage,
}

impl ResponseParts {
    fn apply_item(&mut self, item: OutputItem) {
        match item {
            OutputItem::Message { content } => {
                for part in content {
                    match part {
                        OutputContent::OutputText { text } => self.text.push_str(&text),
                        OutputContent::Refusal { refusal } => self.text.push_str(&refusal),
                        OutputContent::Unknown => {}
                    }
                }
            }
            OutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                if name.is_empty() {
                    return;
                }
                let id = if call_id.is_empty() { id } else { call_id };
                let arguments = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
                if let Some(existing) = self.tool_calls.iter_mut().find(|call| call.id == id) {
                    *existing = ToolCall {
                        id,
                        name,
                        arguments,
                    };
                } else {
                    self.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            OutputItem::Reasoning {
                encrypted_content,
                summary,
            } => {
                for part in summary {
                    self.thinking.push_str(&part.text);
                }
                if encrypted_content.is_some() {
                    self.thinking_signature = encrypted_content;
                }
            }
            OutputItem::Unknown => {}
        }
    }

    fn finish(self) -> ChatResponse {
        let message = if self.tool_calls.is_empty() {
            Message::assistant(self.text)
        } else {
            Message::assistant_with_calls(self.text, self.tool_calls)
        };
        let message = if self.thinking.is_empty() && self.thinking_signature.is_none() {
            message
        } else {
            message.with_thinking(self.thinking, self.thinking_signature)
        };
        ChatResponse::new(message, self.usage)
    }
}

fn parse_response(label: &str, body: &str) -> Result<ChatResponse> {
    let response: ResponsesResponse = decode(label, body)?;
    let mut parts = ResponseParts {
        usage: response.usage.into(),
        ..ResponseParts::default()
    };
    for item in response.output {
        parts.apply_item(item);
    }
    Ok(parts.finish())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamingEvent {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta { delta: String },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta { delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(default)]
        item_id: String,
        delta: String,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OutputItem },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OutputItem },
    #[serde(rename = "response.completed")]
    Completed { response: ResponsesResponse },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponsesResponse },
    #[serde(rename = "error")]
    Error { error: Value },
    #[serde(other)]
    Unknown,
}

#[derive(Default)]
struct PartialCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Folds semantic Open Responses events into one provider-neutral response.
#[derive(Default)]
pub(crate) struct OpenResponsesStream {
    parts: ResponseParts,
    partial_calls: Vec<PartialCall>,
}

impl OpenResponsesStream {
    fn partial_call(&mut self, item_id: &str) -> &mut PartialCall {
        if let Some(index) = self
            .partial_calls
            .iter()
            .position(|call| call.item_id == item_id)
        {
            return &mut self.partial_calls[index];
        }
        self.partial_calls.push(PartialCall {
            item_id: item_id.to_string(),
            ..PartialCall::default()
        });
        self.partial_calls.last_mut().expect("just pushed")
    }

    fn apply_output_item(&mut self, item: OutputItem, done: bool) {
        match item {
            OutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                let item_id = id.clone();
                let partial = self.partial_call(&item_id);
                if !call_id.is_empty() {
                    partial.call_id = call_id;
                }
                if !name.is_empty() {
                    partial.name = name;
                }
                if done || !arguments.is_empty() {
                    partial.arguments = arguments;
                }
            }
            OutputItem::Message { content } => {
                // `output_item.done` repeats the text already delivered as
                // deltas. Use it only as a fallback for non-streaming-looking
                // streams that omitted deltas.
                if self.parts.text.is_empty() {
                    self.parts.apply_item(OutputItem::Message { content });
                }
            }
            OutputItem::Reasoning {
                encrypted_content,
                summary,
            } => {
                if self.parts.thinking.is_empty() {
                    for part in summary {
                        self.parts.thinking.push_str(&part.text);
                    }
                }
                if encrypted_content.is_some() {
                    self.parts.thinking_signature = encrypted_content;
                }
            }
            OutputItem::Unknown => {}
        }
    }

    fn apply_completed(&mut self, response: ResponsesResponse) {
        self.parts.usage = response.usage.into();
        if self.parts.text.is_empty()
            && self.parts.tool_calls.is_empty()
            && self.partial_calls.is_empty()
        {
            for item in response.output {
                self.parts.apply_item(item);
            }
        }
    }
}

impl StreamAccumulator for OpenResponsesStream {
    fn apply(&mut self, data: &str) -> Result<Option<String>> {
        match decode("openresponses", data)? {
            StreamingEvent::OutputTextDelta { delta } => {
                self.parts.text.push_str(&delta);
                Ok(Some(delta))
            }
            StreamingEvent::ReasoningSummaryDelta { delta }
            | StreamingEvent::ReasoningTextDelta { delta } => {
                self.parts.thinking.push_str(&delta);
                Ok(None)
            }
            StreamingEvent::FunctionCallArgumentsDelta { item_id, delta } => {
                self.partial_call(&item_id).arguments.push_str(&delta);
                Ok(None)
            }
            StreamingEvent::OutputItemAdded { item } => {
                self.apply_output_item(item, false);
                Ok(None)
            }
            StreamingEvent::OutputItemDone { item } => {
                self.apply_output_item(item, true);
                Ok(None)
            }
            StreamingEvent::Completed { response } | StreamingEvent::Incomplete { response } => {
                self.apply_completed(response);
                Ok(None)
            }
            StreamingEvent::Error { error } => Err(Error::driver(
                LlmErrorKind::Unknown,
                format!("openresponses stream error: {error}"),
            )),
            StreamingEvent::Unknown => Ok(None),
        }
    }

    fn is_terminator(data: &str) -> bool {
        data == "[DONE]"
    }

    fn text(&self) -> &str {
        &self.parts.text
    }

    fn finish(mut self) -> ChatResponse {
        for call in self.partial_calls {
            if call.name.is_empty() {
                continue;
            }
            self.parts.tool_calls.push(ToolCall {
                id: call.call_id,
                name: call.name,
                arguments: serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({})),
            });
        }
        self.parts.finish()
    }
}

#[async_trait]
impl HttpProvider for OpenResponsesDriver {
    type Accumulator = OpenResponsesStream;

    fn label(&self) -> &str {
        self.label
    }

    fn default_base_url(&self) -> &str {
        self.default_base_url
    }

    fn endpoint(&self) -> &str {
        "/responses"
    }

    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder> {
        if let Some(provider) = &self.auth {
            return http::apply_auth_header(builder, provider.auth_header().await?);
        }
        match &model.api_key {
            Some(key) => Ok(builder.bearer_auth(key)),
            None if self.require_auth => Err(Error::driver(
                LlmErrorKind::Authentication,
                format!("{} driver requires authentication", self.label),
            )),
            None => Ok(builder),
        }
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        request_body(request)
    }

    fn enable_streaming(&self, body: &mut Value) {
        body["stream"] = json!(true);
    }

    fn parse_response(&self, body: &str) -> Result<ChatResponse> {
        parse_response(self.label, body)
    }
}

#[async_trait]
impl ChatDriver for OpenResponsesDriver {
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
        ChatRequest::new(model, vec![Message::user("hi")]).system_prompt("be terse")
    }

    #[test]
    fn builds_responses_input_and_reasoning() {
        let body = request_body(&request(
            ModelSpec::openresponses("gpt-x").reasoning_effort("high"),
        ));
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"], "hi");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn replays_tool_calls_and_results_as_items() {
        let messages = vec![
            Message::assistant_with_calls(
                "",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1}),
                }],
            ),
            Message::tool_result("call_1", "2"),
        ];
        let input = input_items(&messages);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["arguments"], "{\"a\":1}");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    #[test]
    fn parses_text_tools_reasoning_and_usage() {
        let payload = json!({
            "output": [
                {"type": "reasoning", "encrypted_content": "enc", "summary": [
                    {"type": "summary_text", "text": "considered"}
                ]},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "done"}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "add",
                 "arguments": "{\"a\":1}"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let response = parse_response("openresponses", &payload.to_string()).unwrap();
        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.thinking.as_deref(), Some("considered"));
        assert_eq!(response.message.thinking_signature.as_deref(), Some("enc"));
        assert_eq!(response.message.tool_calls[0].name, "add");
        assert_eq!(response.usage, Usage::new(5, 3));
    }

    #[tokio::test]
    async fn streaming_folds_semantic_events() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"add\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"a\\\":1}\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":4}}}\n\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.deltas, vec!["Hel", "lo"]);
        assert_eq!(response.message.text(), "Hello");
        assert_eq!(response.message.tool_calls[0].id, "call_1");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
        assert_eq!(response.usage, Usage::new(2, 4));
    }
}
