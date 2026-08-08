//! The [OpenResponses](https://www.openresponses.org/) protocol — the
//! vendor-neutral standard OpenAI's Responses API implements, and what
//! [`openai()`] speaks by default. Which service a request reaches is a
//! [`Provider`]'s business, not this driver's; OpenRouter and several
//! gateways serve it too.
//!
//! Responses is not a rename of Chat Completions: the conversation is a flat
//! list of typed *items* (`message`, `function_call`, `function_call_output`,
//! `reasoning`) rather than a list of role-tagged messages, the system prompt
//! is `instructions`, and reasoning is a first-class item rather than an
//! invisible token count. That last point is why it is the default — a
//! reasoning model's summary round-trips here and is simply unavailable on
//! Chat Completions.
//!
//! Chat Completions has not gone anywhere: it is still what most
//! OpenAI-compatible vendors, gateways, and local runtimes speak, and reaching
//! them is [`OpenAiDriver`](super::openai::OpenAiDriver) on a provider of your
//! own — or `providers::openai(key).with_driver(OpenAiDriver::new())` for
//! OpenAI itself.
//!
//! Wire mapping only — sending, status/transport classification, and SSE
//! framing live in the crate-internal `drivers::http` layer.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, DeltaSink, Usage};
use agentyk_core::error::{Error, LlmErrorKind, Result};
use agentyk_core::message::{ContentPart, Message, Role, ToolCall};
use agentyk_core::provider::{BearerAuth, Provider, ProviderId};

use super::http::{self, StreamAccumulator, WireProtocol, decode};
use super::openai::{OPENAI_BASE_URL, parse_tool_call_arguments};

/// OpenAI itself: `api.openai.com`, bearer auth, the Responses protocol.
///
/// ```no_run
/// # use agentyk::{Agent, ModelSpec, providers};
/// # fn wire(api_key: String) -> agentyk::Result<()> {
/// let agent = Agent::builder()
///     .name("assistant")
///     .model(ModelSpec::on("openai", "gpt-5.5"))
///     .provider(providers::openai(api_key))
///     .build()?;
/// # Ok(())
/// # }
/// ```
///
/// To talk to OpenAI over Chat Completions instead — an account or a proxy
/// that has not enabled Responses — keep the endpoint and credentials and swap
/// the protocol:
///
/// ```no_run
/// # use agentyk::{OpenAiDriver, providers};
/// let chat_completions = providers::openai("key").with_driver(OpenAiDriver::new());
/// ```
pub fn openai(api_key: impl Into<String>) -> Provider {
    Provider::new(ProviderId::openai(), OpenResponsesDriver::new())
        .base_url(OPENAI_BASE_URL)
        .auth(BearerAuth::new(api_key))
}

/// Speaks the OpenAI Responses protocol. One instance serves any number of
/// services that speak it — pair it with a [`Provider`] that supplies the
/// endpoint and credentials. Streams real incremental deltas.
pub struct OpenResponsesDriver {
    client: reqwest::Client,
    store: bool,
}

impl OpenResponsesDriver {
    /// The protocol on a default HTTP client.
    pub fn new() -> Self {
        Self::with_client(super::http::client())
    }

    /// Supply your own client — timeouts, proxies, and connection pooling are
    /// its business, not the driver's.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            store: false,
        }
    }

    /// Whether OpenAI should retain the response server-side (default:
    /// **off**, where the API's own default is on).
    ///
    /// agentyk replays the whole transcript from its own event log and sends
    /// it in full on every turn, so retention buys the agent nothing while
    /// quietly leaving conversation data on OpenAI's side — not a default a
    /// library should choose for its host. Turn it on for the features that
    /// need server state: `previous_response_id` chaining, or fetching a
    /// response after the fact.
    pub fn store(mut self, store: bool) -> Self {
        self.store = store;
        self
    }
}

impl Default for OpenResponsesDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// Content parts as Responses input content.
///
/// The part *type* depends on the direction: what the model was sent is
/// `input_text` / `input_image`, what it produced is `output_text`. Sending
/// the wrong one back on a replayed assistant turn is rejected, which is why
/// this takes the role rather than guessing.
fn wire_content(role: Role, content: &[ContentPart]) -> Value {
    let text_type = if role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    // A text-only message may travel as a plain string, which is smaller and
    // is what the API documents for the common case.
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
                ContentPart::Text(text) => json!({"type": text_type, "text": text.text}),
                ContentPart::Image(image) => {
                    let url = image.url.clone().unwrap_or_else(|| {
                        format!(
                            "data:{};base64,{}",
                            image.media_type.as_deref().unwrap_or("image/png"),
                            image.base64.as_deref().unwrap_or_default()
                        )
                    });
                    json!({"type": "input_image", "image_url": url})
                }
            })
            .collect(),
    )
}

/// Expand one message into the input items it needs.
///
/// The count varies by role, which is the shape of the protocol rather than a
/// quirk: an assistant turn that both spoke and called two tools is three
/// items, and a tool result carrying images is a `function_call_output` plus a
/// user message to carry them (the output field is text only — the same
/// deliberate workaround the Chat Completions driver makes, for the same
/// reason).
fn to_wire_items(message: &Message) -> Vec<Value> {
    match message.role {
        // The agent's own system prompt travels via `instructions`; a system
        // message *in the transcript* is someone's deliberate instruction and
        // is kept, under the role this protocol names for it.
        Role::System => vec![json!({
            "type": "message",
            "role": "developer",
            "content": wire_content(Role::System, &message.content),
        })],
        Role::User => vec![json!({
            "type": "message",
            "role": "user",
            "content": wire_content(Role::User, &message.content),
        })],
        Role::Assistant => {
            let mut items = Vec::new();
            // An assistant turn that only called tools has no message item —
            // an empty one is not something the model ever produced.
            if !message.text().is_empty() {
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": wire_content(Role::Assistant, &message.content),
                }));
            }
            for call in &message.tool_calls {
                items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments.to_string(),
                }));
            }
            items
        }
        Role::Tool => {
            let mut items = vec![json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id,
                "output": message.text(),
            })];
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
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": wire_content(Role::User, &content),
                }));
            }
            items
        }
    }
}

/// Responses reports totals with itemized breakdowns, the same way Chat
/// Completions does under different names: `input_tokens` already includes
/// cached tokens, `output_tokens` already includes reasoning tokens.
#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: WireInputDetails,
    #[serde(default)]
    output_tokens_details: WireOutputDetails,
}

#[derive(Debug, Default, Deserialize)]
struct WireInputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
struct WireOutputDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Usage::new(usage.input_tokens, usage.output_tokens)
            // OpenAI caches automatically, so a host that never asked for
            // caching still needs these to price a request correctly.
            .with_cache(usage.input_tokens_details.cached_tokens, 0)
            .with_reasoning(usage.output_tokens_details.reasoning_tokens)
    }
}

/// One item of a response's `output` array.
///
/// Unknown item *types* deserialize to [`OutputItem::Other`] rather than
/// failing: OpenAI adds them (`web_search_call`, `image_generation_call`, …)
/// and a response carrying one is still perfectly usable. A known item whose
/// *fields* changed does fail, which is the distinction worth having.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    Message {
        #[serde(default)]
        content: Vec<OutputContent>,
    },
    FunctionCall {
        /// The id a `function_call_output` must quote — not the item's own
        /// `id`, which is a different value the API rejects here.
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    Reasoning {
        #[serde(default)]
        summary: Vec<SummaryPart>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputContent {
    OutputText {
        text: String,
    },
    /// A declined request. Kept as answer text rather than raised as an error:
    /// a refusal is something the model *said*, and a host that cannot show it
    /// to the user has lost the only explanation there is.
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SummaryPart {
    SummaryText {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// A non-streaming Responses response.
///
/// `output` is required, and its absence means the shape we depend on has
/// changed — reporting that beats handing the model a blank answer. `usage` is
/// defaulted, because losing token counts is a reporting gap, not a wrong
/// answer.
#[derive(Debug, Deserialize)]
struct ResponseBody {
    output: Vec<OutputItem>,
    #[serde(default)]
    usage: WireUsage,
}

/// Fold output items into an assistant [`Message`]: concatenated text,
/// `function_call` items, and any reasoning summary (kept on `thinking`, with
/// no signature — Responses does not issue one).
fn message_from_output(output: Vec<OutputItem>) -> Message {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item {
            OutputItem::Message { content } => {
                for part in content {
                    match part {
                        OutputContent::OutputText { text: part } => text.push_str(&part),
                        OutputContent::Refusal { refusal } => text.push_str(&refusal),
                        OutputContent::Other => {}
                    }
                }
            }
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => tool_calls.push(ToolCall {
                id: call_id,
                name,
                arguments: parse_tool_call_arguments(&Value::String(arguments)),
            }),
            OutputItem::Reasoning { summary } => {
                for part in summary {
                    if let SummaryPart::SummaryText { text: part } = part {
                        thinking.push_str(&part);
                    }
                }
            }
            OutputItem::Other => {}
        }
    }
    let message = Message::assistant_with_calls(text, tool_calls);
    if thinking.is_empty() {
        message
    } else {
        message.with_thinking(thinking, None)
    }
}

#[derive(Default)]
struct PartialToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// One streaming event. Unknown event types deserialize to `Other` — OpenAI
/// adds them — while a known one whose shape changed fails loudly.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    /// A text fragment of the answer.
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    /// A refusal fragment — the model's own words for why it declined, so it
    /// is answer text like any other (see [`OutputContent::Refusal`]).
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta { delta: String },
    /// A reasoning-summary fragment: accumulated, never forwarded as answer
    /// text.
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta { delta: String },
    /// Plaintext reasoning, as OpenAI-compatible gateways emit it for open
    /// reasoning models. Same destination as the summary above — it is
    /// reasoning, not the answer.
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta { delta: String },
    /// A new output item opened. For a `function_call` this is where its
    /// `call_id` and `name` arrive — the argument fragments that follow carry
    /// only the index.
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(default)]
        output_index: usize,
        item: StreamItem,
    },
    /// An output item closed. Carries the complete item, which is the only
    /// source of a function call's arguments on servers that never send
    /// fragments.
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        #[serde(default)]
        output_index: usize,
        item: StreamItem,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(default)]
        output_index: usize,
        delta: String,
    },
    /// The complete argument string for one call — the fallback for a server
    /// that streamed no fragments.
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        #[serde(default)]
        output_index: usize,
        arguments: String,
    },
    /// The terminal event, and where usage for the whole response arrives.
    #[serde(rename = "response.completed")]
    Completed { response: CompletedResponse },
    /// Terminal too: the model stopped early (a token cap, a filter). The
    /// answer so far is real and is kept — only the usage matters here.
    #[serde(rename = "response.incomplete")]
    Incomplete { response: CompletedResponse },
    /// The provider gave up mid-stream. Without this the turn would end with
    /// whatever text had arrived and no indication anything went wrong.
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    /// A transport- or provider-level error delivered as an event rather than
    /// a status code — same reasoning as `response.failed`.
    #[serde(rename = "error")]
    Error { error: WireError },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Debug, Deserialize)]
struct FailedResponse {
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Default, Deserialize)]
struct WireError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: String,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{code}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamItem {
    FunctionCall {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Other,
}

/// Accumulates a Responses streaming response: text fragments concatenate
/// directly; function-call arguments are JSON-fragment text keyed by
/// `output_index` and only parsed once complete, at
/// [`StreamAccumulator::finish`].
#[derive(Default)]
pub(crate) struct OpenResponsesStream {
    text: String,
    thinking: String,
    /// Keyed by `output_index` and ordered by it, so tool calls come back in
    /// the order the model emitted them.
    calls: BTreeMap<usize, PartialToolCall>,
    usage: Usage,
}

impl StreamAccumulator for OpenResponsesStream {
    fn apply(&mut self, data: &str) -> Result<Option<String>> {
        let event: StreamEvent = decode("openai responses", data)?;
        Ok(match event {
            StreamEvent::OutputTextDelta { delta } => {
                self.text.push_str(&delta);
                Some(delta)
            }
            StreamEvent::RefusalDelta { delta } => {
                self.text.push_str(&delta);
                Some(delta)
            }
            StreamEvent::ReasoningSummaryDelta { delta }
            | StreamEvent::ReasoningTextDelta { delta } => {
                self.thinking.push_str(&delta);
                None
            }
            StreamEvent::OutputItemAdded {
                output_index,
                item:
                    StreamItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    },
            } => {
                let entry = self.calls.entry(output_index).or_default();
                entry.call_id = call_id;
                entry.name = name;
                entry.arguments.push_str(&arguments);
                None
            }
            StreamEvent::OutputItemDone {
                output_index,
                item:
                    StreamItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    },
            } => {
                let entry = self.calls.entry(output_index).or_default();
                if entry.call_id.is_empty() {
                    entry.call_id = call_id;
                }
                if entry.name.is_empty() {
                    entry.name = name;
                }
                // The done event repeats the whole argument string, so it is
                // a fallback for a server that streamed no fragments — never
                // an append, which would double them.
                if entry.arguments.is_empty() {
                    entry.arguments = arguments;
                }
                None
            }
            StreamEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
            } => {
                self.calls
                    .entry(output_index)
                    .or_default()
                    .arguments
                    .push_str(&delta);
                None
            }
            StreamEvent::FunctionCallArgumentsDone {
                output_index,
                arguments,
            } => {
                let entry = self.calls.entry(output_index).or_default();
                // Repeats the whole string rather than continuing it — a
                // fallback, never an append, which would double the JSON.
                if entry.arguments.is_empty() {
                    entry.arguments = arguments;
                }
                None
            }
            StreamEvent::Completed { response } | StreamEvent::Incomplete { response } => {
                self.usage = response.usage.into();
                None
            }
            StreamEvent::Failed { response } => {
                return Err(Error::driver(
                    LlmErrorKind::ServerError,
                    format!(
                        "openai responses stream failed: {}",
                        response.error.unwrap_or_default()
                    ),
                ));
            }
            StreamEvent::Error { error } => {
                return Err(Error::driver(
                    LlmErrorKind::Unknown,
                    format!("openai responses stream reported an error: {error}"),
                ));
            }
            StreamEvent::OutputItemAdded { .. }
            | StreamEvent::OutputItemDone { .. }
            | StreamEvent::Other => None,
        })
    }

    /// Some gateways close a Responses stream with the Chat Completions
    /// sentinel, which is not JSON and must not be parsed as an event.
    fn is_terminator(data: &str) -> bool {
        data == "[DONE]"
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn finish(self) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCall {
                id: call.call_id,
                name: call.name,
                arguments: parse_tool_call_arguments(&Value::String(call.arguments)),
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
            message.with_thinking(self.thinking, None)
        };
        ChatResponse::new(message, self.usage)
    }
}

impl WireProtocol for OpenResponsesDriver {
    type Accumulator = OpenResponsesStream;

    fn label(&self) -> &str {
        "openai responses"
    }

    fn endpoint(&self) -> &str {
        "/responses"
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let input: Vec<Value> = request.messages.iter().flat_map(to_wire_items).collect();

        let mut body = json!({
            "model": request.model.model,
            "input": input,
            "store": self.store,
        });
        if let Some(system) = &request.system_prompt {
            body["instructions"] = json!(system);
        }
        if let Some(effort) = request
            .model
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.as_deref())
            // `none` means "do not reason", and the way to ask for that is to
            // send no reasoning block at all — sending one is an API error on
            // models that have no reasoning to turn off.
            .filter(|effort| !effort.eq_ignore_ascii_case("none"))
        {
            // Ask for the summary too: without it a reasoning response carries
            // an opaque item and the token count, and `Message::thinking`
            // stays empty.
            //
            // `auto` is the model's discretion, not a guarantee — a live
            // gpt-5.5 call at `low` effort reports reasoning *tokens* and emits
            // no summary events at all, so `thinking` is legitimately empty
            // there while `high` fills it. Worth knowing before suspecting this
            // driver of dropping it.
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
        }
        if !request.tools.is_empty() {
            // Flat, unlike Chat Completions' nested `function` object.
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        })
                    })
                    .collect(),
            );
        }
        body
    }

    /// Usage rides the terminal `response.completed` event, so there is no
    /// opt-in to add here.
    fn enable_streaming(&self, body: &mut Value) {
        body["stream"] = json!(true);
    }

    fn parse_response(&self, body: &str) -> Result<ChatResponse> {
        let ResponseBody { output, usage } = decode(self.label(), body)?;
        Ok(ChatResponse::new(message_from_output(output), usage.into()))
    }
}

#[async_trait]
impl ChatDriver for OpenResponsesDriver {
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
    use agentyk_core::driver::ModelSpec;

    fn request(model: ModelSpec) -> ChatRequest {
        ChatRequest::new(model, vec![Message::user("hi")])
    }

    fn tool(name: &str) -> agentyk_core::tool::ToolDefinition {
        agentyk_core::tool::ToolDefinition::new(name, "does a thing", json!({"type": "object"}))
    }

    #[test]
    fn the_system_prompt_becomes_instructions_not_an_input_item() {
        let mut request = request(ModelSpec::openai("gpt-5.5"));
        request.system_prompt = Some("be terse".into());
        let body = OpenResponsesDriver::new().build_body(&request);

        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn tools_are_flat_rather_than_nested_under_function() {
        let mut request = request(ModelSpec::openai("gpt-5.5"));
        request.tools = vec![tool("add")];
        let body = OpenResponsesDriver::new().build_body(&request);

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "add");
        assert!(body["tools"][0]["function"].is_null());
    }

    /// The transcript is replayed from agentyk's own log every turn, so
    /// server-side retention is off unless a host asks for it.
    #[test]
    fn responses_are_not_stored_server_side_by_default() {
        let driver = OpenResponsesDriver::new();
        assert_eq!(
            driver.build_body(&request(ModelSpec::openai("gpt-5.5")))["store"],
            false
        );
        assert_eq!(
            driver
                .store(true)
                .build_body(&request(ModelSpec::openai("gpt-5.5")))["store"],
            true
        );
    }

    #[test]
    fn reasoning_effort_asks_for_a_summary_so_thinking_is_not_lost() {
        let body = OpenResponsesDriver::new().build_body(&request(
            ModelSpec::openai("gpt-5.5").reasoning_effort("high"),
        ));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn reasoning_omitted_when_unset() {
        let body = OpenResponsesDriver::new().build_body(&request(ModelSpec::openai("gpt-5.5")));
        assert!(body.get("reasoning").is_none());
    }

    /// "Don't reason" is expressed by sending no reasoning block — sending one
    /// that says `none` is an API error on models with nothing to turn off.
    #[test]
    fn effort_none_sends_no_reasoning_block() {
        let body = OpenResponsesDriver::new().build_body(&request(
            ModelSpec::openai("gpt-5.5").reasoning_effort("none"),
        ));
        assert!(body.get("reasoning").is_none());
    }

    /// A system message in the transcript is someone's deliberate instruction;
    /// dropping it would silently change what the model was told.
    #[test]
    fn a_system_message_in_the_transcript_becomes_a_developer_item() {
        // Nothing in agentyk emits one today, but `Message.role` is public and
        // a replayed log can carry it.
        let mut message = Message::user("never guess");
        message.role = Role::System;

        let items = to_wire_items(&message);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "developer");
        assert_eq!(items[0]["content"], "never guess");
    }

    /// The id contract, end to end: what `parse_response` reads off a
    /// `function_call` is what the next turn must quote back.
    #[test]
    fn a_parsed_tool_call_replays_under_the_id_the_api_expects() {
        let payload = json!({
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "add",
                "arguments": "{\"a\":1}",
            }],
        })
        .to_string();
        let response = OpenResponsesDriver::new().parse_response(&payload).unwrap();

        let call = to_wire_items(&response.message);
        let result = to_wire_items(&Message::tool_result(
            &response.message.tool_calls[0].id,
            "2",
        ));

        assert_eq!(call[0]["call_id"], "call_1");
        assert_eq!(result[0]["call_id"], "call_1");
    }

    /// A refusal is something the model said, not a transport failure — a host
    /// that cannot show it has lost the only explanation there is.
    #[test]
    fn a_refusal_arrives_as_answer_text() {
        let payload = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "refusal", "refusal": "I can't help with that."}],
            }],
        })
        .to_string();
        let response = OpenResponsesDriver::new().parse_response(&payload).unwrap();
        assert_eq!(response.message.text(), "I can't help with that.");
    }

    /// Input content is typed by direction: what the model was sent is
    /// `input_*`, what it produced is `output_text`. The wrong one is rejected.
    #[test]
    fn replayed_content_is_typed_by_direction() {
        let user = wire_content(
            Role::User,
            &[
                ContentPart::text("what is this?"),
                ContentPart::image_url("https://example.com/cat.png"),
            ],
        );
        assert_eq!(user[0]["type"], "input_text");
        assert_eq!(user[1]["type"], "input_image");
        assert_eq!(user[1]["image_url"], "https://example.com/cat.png");

        let assistant = wire_content(
            Role::Assistant,
            &[
                ContentPart::text("a cat"),
                ContentPart::image_url("https://example.com/cat.png"),
            ],
        );
        assert_eq!(assistant[0]["type"], "output_text");
    }

    #[test]
    fn text_only_content_serializes_as_a_plain_string() {
        assert_eq!(
            wire_content(Role::User, &[ContentPart::text("hello")]),
            json!("hello")
        );
    }

    /// An assistant turn is a *list* of items here, not one message with a
    /// `tool_calls` field — and a turn that only called tools has no message
    /// item at all.
    #[test]
    fn an_assistant_turn_expands_into_message_and_function_call_items() {
        let spoke_and_called = to_wire_items(&Message::assistant_with_calls(
            "on it",
            vec![ToolCall {
                id: "call_0".into(),
                name: "add".into(),
                arguments: json!({"a": 1}),
            }],
        ));
        assert_eq!(spoke_and_called.len(), 2);
        assert_eq!(spoke_and_called[0]["role"], "assistant");
        assert_eq!(spoke_and_called[1]["type"], "function_call");
        assert_eq!(spoke_and_called[1]["call_id"], "call_0");
        // Arguments travel as a JSON *string*, as they do on the way back.
        assert_eq!(spoke_and_called[1]["arguments"], "{\"a\":1}");

        let only_called = to_wire_items(&Message::assistant_with_calls(
            "",
            vec![ToolCall {
                id: "call_0".into(),
                name: "add".into(),
                arguments: json!({}),
            }],
        ));
        assert_eq!(only_called.len(), 1);
        assert_eq!(only_called[0]["type"], "function_call");
    }

    #[test]
    fn a_tool_result_quotes_the_call_id_it_answers() {
        let items = to_wire_items(&Message::tool_result("call_0", "42"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_0");
        assert_eq!(items[0]["output"], "42");
    }

    /// `output` is text only, so images a tool returned follow as a user
    /// message rather than being dropped.
    #[test]
    fn images_in_a_tool_result_follow_as_a_user_message() {
        let message = Message::tool_result_with_parts(
            "call_0",
            "here",
            vec![ContentPart::image_base64("Zm9v", "image/png")],
        );
        let items = to_wire_items(&message);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[1]["role"], "user");
        assert_eq!(items[1]["content"][1]["type"], "input_image");
    }

    #[test]
    fn parse_response_reads_text_reasoning_calls_and_usage() {
        let payload = json!({
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "let me see"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "add", "arguments": "{\"a\":1}"},
            ],
            "usage": {"input_tokens": 5, "output_tokens": 9},
        })
        .to_string();

        let response = OpenResponsesDriver::new().parse_response(&payload).unwrap();

        assert_eq!(response.message.text(), "done");
        assert_eq!(response.message.thinking.as_deref(), Some("let me see"));
        // The `call_id`, not the item's `id` — quoting the wrong one is
        // rejected on the next turn.
        assert_eq!(response.message.tool_calls[0].id, "call_1");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
        assert_eq!(response.usage, Usage::new(5, 9));
    }

    /// Totals stay totals; the itemization is kept as a breakdown so a host
    /// can price the request and see whether the automatic cache hit.
    #[test]
    fn parse_response_keeps_the_cache_and_reasoning_breakdown() {
        let payload = json!({
            "output": [],
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 40,
                "input_tokens_details": {"cached_tokens": 1024},
                "output_tokens_details": {"reasoning_tokens": 32},
            },
        })
        .to_string();

        let usage = OpenResponsesDriver::new()
            .parse_response(&payload)
            .unwrap()
            .usage;

        assert_eq!(usage.input_tokens, 1200);
        assert_eq!(usage.cache_read_input_tokens, 1024);
        assert_eq!(usage.reasoning_tokens, 32);
    }

    /// An unrecognized item type is expected — OpenAI adds them — and must not
    /// cost us the rest of the response.
    #[test]
    fn parse_response_ignores_unknown_item_types() {
        let payload = json!({
            "output": [
                {"type": "web_search_call", "id": "ws_1", "status": "completed"},
                {"type": "message", "content": [{"type": "output_text", "text": "answer"}]},
            ],
        })
        .to_string();
        let response = OpenResponsesDriver::new().parse_response(&payload).unwrap();
        assert_eq!(response.message.text(), "answer");
    }

    #[test]
    fn parse_response_reports_a_changed_shape_instead_of_returning_nothing() {
        let payload = json!({"choices": [{"message": {"content": "hi"}}]}).to_string();
        let error = OpenResponsesDriver::new()
            .parse_response(&payload)
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("did not match the expected shape"),
            "{message}"
        );
        assert!(message.contains("output"), "{message}");
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn streaming_reports_deltas_and_returns_the_final_message() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.created\",\"response\":{}}\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n",
                // Split mid-line, to prove reassembly through the real loop.
                "data: {\"type\":\"response.output_text.de",
                "lta\",\"delta\":\"lo\"}\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n",
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
    async fn streaming_assembles_tool_calls_by_output_index() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_0\",\"name\":\"add\",\"arguments\":\"\"}}\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"a\\\":\"}\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"1}\"}\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_0\",\"name\":\"add\",\"arguments\":\"{\\\"a\\\":1}\"}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert!(sink.deltas.is_empty(), "tool JSON is not answer text");
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].id, "call_0");
        assert_eq!(response.message.tool_calls[0].name, "add");
        // Not `{"a":1}{"a":1}`: the done event repeats the arguments rather
        // than continuing them.
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 1}));
    }

    /// A server that sends no argument fragments still yields a usable call.
    #[tokio::test]
    async fn streaming_falls_back_to_the_completed_item_for_arguments() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"add\",\"arguments\":\"{\\\"a\\\":2}\"}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(response.message.tool_calls[0].id, "call_9");
        assert_eq!(response.message.tool_calls[0].arguments, json!({"a": 2}));
    }

    #[tokio::test]
    async fn streaming_collects_the_reasoning_summary_without_polluting_the_answer() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"hmm\"}\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.deltas, vec!["answer"], "reasoning is not answer text");
        assert_eq!(response.message.text(), "answer");
        assert_eq!(response.message.thinking.as_deref(), Some("hmm"));
    }

    /// Gateways serving this protocol emit plaintext reasoning under their own
    /// event name; it belongs where the summary goes, not in the answer.
    #[tokio::test]
    async fn streaming_collects_gateway_plaintext_reasoning() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"delta\":\"User asks\"}\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.deltas, vec!["answer"]);
        assert_eq!(response.message.thinking.as_deref(), Some("User asks"));
    }

    /// Some gateways close a Responses stream with the Chat Completions
    /// sentinel. It is not JSON, and parsing it as an event would turn a
    /// perfectly good response into a decode failure.
    #[tokio::test]
    async fn streaming_tolerates_a_chat_completions_done_sentinel() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n",
                "data: [DONE]\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(response.message.text(), "hi");
    }

    /// A provider that gives up mid-stream must not read as a short answer.
    #[tokio::test]
    async fn streaming_surfaces_a_failure_event() {
        let mut sink = RecordingSink::default();
        let error = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"error\":{\"code\":\"server_error\",\"message\":\"upstream gave up\"}}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("upstream gave up"), "{error}");
        assert!(error.is_retryable(), "a server error is worth retrying");
    }

    #[tokio::test]
    async fn streaming_surfaces_an_error_event() {
        let mut sink = RecordingSink::default();
        let error = drive_stream::<OpenResponsesStream>(
            &["data: {\"type\":\"error\",\"error\":{\"code\":\"rate_limit\",\"message\":\"slow down\"}}\n"],
            &mut sink,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("slow down"), "{error}");
    }

    /// An early stop is a real answer plus a real token count, not a failure.
    #[tokio::test]
    async fn streaming_keeps_the_answer_and_usage_of_an_incomplete_response() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"as far as\"}\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":4,\"output_tokens\":8}}}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(response.message.text(), "as far as");
        assert_eq!(response.usage, Usage::new(4, 8));
    }

    /// OpenAI adds event types constantly; a stream carrying one is still a
    /// perfectly good stream.
    #[tokio::test]
    async fn streaming_ignores_unknown_event_types() {
        let mut sink = RecordingSink::default();
        let response = drive_stream::<OpenResponsesStream>(
            &[
                "data: {\"type\":\"response.in_progress\",\"response\":{}}\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n",
            ],
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(response.message.text(), "ok");
    }
}
