//! The HTTP drivers against a real socket.
//!
//! `SimDriver` proves the turn loop; it cannot prove that a driver builds a
//! request, reads a response, or folds a stream — the code between reqwest and
//! `ChatResponse` had only unit coverage of its parts. These tests serve
//! canned provider responses from a local `TcpListener` and drive the real
//! [`ChatDriver`] through a [`Provider`] pointed at that socket, so the
//! request actually goes out over the wire and the response actually comes
//! back through the shared HTTP layer.
//!
//! What they still cannot prove is that the canned bodies match what the
//! providers really send today. Only a live call does that.

#![cfg(feature = "http")]

use std::sync::Arc;

use agentyk::{
    Agent, ChatRequest, DeltaSink, Message, ModelSpec, OpenAiDriver, Provider, Result, Tool,
    ToolContext, ToolDefinition, ToolOutput, providers,
};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Serves canned responses in order — one per request — and hands back the
/// requests it received so a test can assert on what the driver actually sent.
struct FakeProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeProvider {
    async fn serving(status: u16, content_type: &str, body: &str) -> Self {
        Self::serving_all(status, content_type, &[body]).await
    }

    /// One canned response per turn, in order. A multi-turn agent run needs
    /// this: the second request is the one carrying the replayed tool call.
    async fn serving_all(status: u16, content_type: &str, bodies: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));

        let responses: Vec<String> = bodies
            .iter()
            .map(|body| {
                format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            })
            .collect();
        let seen = requests.clone();
        let handle = tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                // Read until the body is in hand: headers, then Content-Length
                // bytes. Good enough for a test client we control.
                let mut buffer = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer);
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("Content-Length: ")
                                    .or_else(|| line.strip_prefix("content-length: "))
                            })
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if rest.len() >= length {
                            break;
                        }
                    }
                }
                seen.lock()
                    .await
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            handle,
        }
    }

    async fn raw_request(&self, index: usize) -> String {
        self.requests
            .lock()
            .await
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("request {index} arrived"))
    }

    /// The body the driver sent, parsed.
    async fn sent_body(&self) -> serde_json::Value {
        self.sent_body_at(0).await
    }

    async fn sent_body_at(&self, index: usize) -> serde_json::Value {
        let raw = self.raw_request(index).await;
        let (_, body) = raw.split_once("\r\n\r\n").expect("headers then body");
        serde_json::from_str(body).expect("the driver sent JSON")
    }

    async fn sent_headers(&self) -> String {
        let raw = self.raw_request(0).await;
        raw.split_once("\r\n\r\n").expect("headers").0.to_string()
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Default)]
struct Collect(Vec<String>);

#[async_trait]
impl DeltaSink for Collect {
    async fn delta(&mut self, delta: &str, _accumulated: &str) -> Result<()> {
        self.0.push(delta.to_string());
        Ok(())
    }
}

fn request(model: ModelSpec) -> ChatRequest {
    ChatRequest::new(model, vec![Message::user("hi")]).system_prompt("be terse")
}

/// A tool for the round-trip test — the arithmetic is beside the point, the
/// `call_id` it answers is not.
struct AddTool;

#[async_trait]
impl Tool for AddTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "add",
            "Add two numbers.",
            serde_json::json!({
                "type": "object",
                "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                "required": ["a", "b"],
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        let sum = arguments["a"].as_i64().unwrap_or(0) + arguments["b"].as_i64().unwrap_or(0);
        ToolOutput::text(sum.to_string())
    }
}

#[tokio::test]
async fn anthropic_completes_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "application/json",
        r#"{"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":4,"output_tokens":2}}"#,
    )
    .await;

    let response = providers::anthropic("k")
        .base_url(&server.base_url)
        .complete(request(ModelSpec::anthropic("claude-x")))
        .await
        .expect("a well-formed response parses");

    assert_eq!(response.message.text(), "hello");
    assert_eq!(response.usage.input_tokens, 4);
    assert_eq!(response.usage.output_tokens, 2);

    // The request the driver built actually went out: right path, right
    // auth header, system prompt hoisted out of the message list.
    let headers = server.sent_headers().await;
    assert!(headers.starts_with("POST /v1/messages "), "{headers}");
    assert!(headers.contains("x-api-key: k"), "{headers}");
    assert!(headers.contains("anthropic-version: "), "{headers}");
    let body = server.sent_body().await;
    // Prompt caching (on by default) sends the system prompt as a block array
    // so it can carry a `cache_control` marker.
    assert_eq!(body["system"][0]["text"], "be terse");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][0]["role"], "user");
}

#[tokio::test]
async fn anthropic_streams_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "text/event-stream",
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"He\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ),
    )
    .await;

    let mut sink = Collect::default();
    let response = providers::anthropic("k")
        .base_url(&server.base_url)
        .complete_streaming(request(ModelSpec::anthropic("claude-x")), &mut sink)
        .await
        .expect("the stream folds");

    assert_eq!(sink.0, vec!["He", "llo"]);
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.usage.output_tokens, 3);
    assert_eq!(server.sent_body().await["stream"], true);
}

#[tokio::test]
async fn openai_completes_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "application/json",
        r#"{"choices":[{"message":{"content":"hey","tool_calls":[{"id":"c1","function":{"name":"add","arguments":"{\"a\":1}"}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":1}}"#,
    )
    .await;

    // No auth on this provider: the local-runtime case.
    let response = Provider::new("local", OpenAiDriver::new())
        .base_url(&server.base_url)
        .complete(request(ModelSpec::on("local", "gpt-x")))
        .await
        .expect("a well-formed response parses");

    assert_eq!(response.message.text(), "hey");
    assert_eq!(response.message.tool_calls[0].name, "add");
    assert_eq!(
        response.message.tool_calls[0].arguments,
        serde_json::json!({"a": 1})
    );
    assert_eq!(response.usage.input_tokens, 7);

    let headers = server.sent_headers().await;
    assert!(headers.starts_with("POST /chat/completions "), "{headers}");
    // No key configured, so no auth header — the local-runtime case.
    assert!(
        !headers.to_lowercase().contains("authorization"),
        "{headers}"
    );
}

#[tokio::test]
async fn openai_streams_chat_completions_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "text/event-stream",
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        ),
    )
    .await;

    let mut sink = Collect::default();
    // The documented escape hatch: OpenAI's endpoint and credentials, the
    // older protocol.
    let response = providers::openai("k")
        .with_driver(OpenAiDriver::new())
        .base_url(&server.base_url)
        .complete_streaming(request(ModelSpec::openai("gpt-x")), &mut sink)
        .await
        .expect("the stream folds");

    assert_eq!(sink.0, vec!["He", "llo"]);
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.usage.output_tokens, 5);
    let headers = server.sent_headers().await;
    assert!(headers.starts_with("POST /chat/completions "), "{headers}");
    let body = server.sent_body().await;
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

/// What `providers::openai` speaks now — the same assembly, a different
/// protocol on the wire.
#[tokio::test]
async fn openai_completes_over_responses_by_default() {
    let server = FakeProvider::serving(
        200,
        "application/json",
        r#"{"id":"resp_1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hey"}]},{"type":"function_call","id":"fc_1","call_id":"call_1","name":"add","arguments":"{\"a\":1}"}],"usage":{"input_tokens":7,"output_tokens":1}}"#,
    )
    .await;

    let response = providers::openai("k")
        .base_url(&server.base_url)
        .complete(request(ModelSpec::openai("gpt-x")))
        .await
        .expect("a well-formed response parses");

    assert_eq!(response.message.text(), "hey");
    assert_eq!(response.message.tool_calls[0].id, "call_1");
    assert_eq!(
        response.message.tool_calls[0].arguments,
        serde_json::json!({"a": 1})
    );
    assert_eq!(response.usage.input_tokens, 7);

    let headers = server.sent_headers().await;
    assert!(headers.starts_with("POST /responses "), "{headers}");
    assert!(headers.contains("authorization: Bearer k"), "{headers}");
    let body = server.sent_body().await;
    // The system prompt is `instructions`, and the conversation is `input`.
    assert_eq!(body["instructions"], "be terse");
    assert_eq!(body["input"][0]["role"], "user");
    assert!(body.get("messages").is_none());
}

#[tokio::test]
async fn openai_streams_responses_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "text/event-stream",
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":5}}}\n\n",
        ),
    )
    .await;

    let mut sink = Collect::default();
    let response = providers::openai("k")
        .base_url(&server.base_url)
        .complete_streaming(request(ModelSpec::openai("gpt-x")), &mut sink)
        .await
        .expect("the stream folds");

    assert_eq!(sink.0, vec!["He", "llo"]);
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.usage.output_tokens, 5);
    assert_eq!(server.sent_body().await["stream"], true);
}

/// The whole tool round trip through the engine, on the Responses wire.
///
/// This is what no unit test reaches: the model asks for a tool by `call_id`,
/// the engine runs it, and the *next* request must replay that call and its
/// output as two separate items quoting the same id. Getting either wrong is a
/// 400 from OpenAI, and only a second turn shows it.
#[tokio::test]
async fn a_tool_call_round_trips_through_a_turn_on_the_responses_wire() {
    // A session streams, so these are event streams rather than whole bodies.
    let server = FakeProvider::serving_all(
        200,
        "text/event-stream",
        &[
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_add_1\",\"name\":\"add\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"a\\\":2,\\\"b\\\":3}\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}}\n\n",
            ),
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"5\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n",
            ),
        ],
    )
    .await;

    let agent = Agent::builder()
        .model(ModelSpec::openai("gpt-x"))
        .provider(providers::openai("k").base_url(&server.base_url))
        .tool(AddTool)
        .build()
        .expect("the agent builds");

    let turn = agent
        .session()
        .run("what is 2 + 3?")
        .await
        .expect("the turn completes");

    assert_eq!(turn.response, "5");

    // The second request is the interesting one: user message, the assistant's
    // function_call, and the tool's function_call_output — flat items, in
    // order, both quoting the call_id the model issued.
    let input = server.sent_body_at(1).await;
    let items = input["input"].as_array().expect("input items");
    assert_eq!(items.len(), 3, "{items:#?}");
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[1]["type"], "function_call");
    assert_eq!(items[1]["call_id"], "call_add_1");
    assert_eq!(items[2]["type"], "function_call_output");
    assert_eq!(items[2]["call_id"], "call_add_1");
    assert_eq!(items[2]["output"], "5");
    // And the tool the model was offered is flat, not nested under `function`.
    assert_eq!(input["tools"][0]["name"], "add");
}

/// An error status is classified rather than parsed — and the body, which may
/// echo the request, is reported as the provider's message, not swallowed.
#[tokio::test]
async fn an_http_error_is_classified_by_retryability() {
    let server = FakeProvider::serving(429, "application/json", r#"{"error":"slow down"}"#).await;

    // Reported against the service, not the protocol: an operator reading
    // this needs to know whose rate limit was hit.
    let error = Provider::new("some-gateway", OpenAiDriver::new())
        .base_url(&server.base_url)
        .complete(request(ModelSpec::on("some-gateway", "gpt-x")))
        .await
        .expect_err("429 is an error");

    assert!(error.is_retryable(), "a rate limit is worth retrying");
    assert!(error.to_string().contains("429"), "{error}");
    assert!(error.to_string().contains("some-gateway"), "{error}");
}

/// The diagnosable-shape-change guarantee, proved through the real transport
/// rather than by calling the parser directly.
#[tokio::test]
async fn a_changed_response_shape_reports_itself_end_to_end() {
    let server = FakeProvider::serving(
        200,
        "application/json",
        r#"{"blocks":[{"type":"text","text":"hello"}]}"#,
    )
    .await;

    let error = providers::anthropic("k")
        .base_url(&server.base_url)
        .complete(request(ModelSpec::anthropic("claude-x")))
        .await
        .expect_err("a renamed field must not read as an empty answer");

    assert!(
        error
            .to_string()
            .contains("did not match the expected shape"),
        "{error}"
    );
    assert!(error.to_string().contains("content"), "{error}");
    assert!(!error.is_retryable(), "a shape change will not fix itself");
}
