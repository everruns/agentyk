//! The HTTP drivers against a real socket.
//!
//! `SimDriver` proves the turn loop; it cannot prove that a driver builds a
//! request, reads a response, or folds a stream — the code between reqwest and
//! `ChatResponse` had only unit coverage of its parts. These tests serve
//! canned provider responses from a local `TcpListener` and drive the real
//! [`ChatDriver`] through [`ModelSpec::base_url`], so the request actually
//! goes out over a socket and the response actually comes back through the
//! shared HTTP layer.
//!
//! What they still cannot prove is that the canned bodies match what the
//! providers really send today. Only a live call does that.

#![cfg(feature = "http")]

use std::sync::Arc;

use agentyk::{
    AnthropicDriver, ChatDriver, ChatRequest, DeltaSink, Message, ModelSpec, OpenAiDriver, Result,
};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Serves one canned response, then hands back the request it received so a
/// test can assert on what the driver actually sent.
struct FakeProvider {
    base_url: String,
    request: Arc<Mutex<Option<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeProvider {
    async fn serving(status: u16, content_type: &str, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let request = Arc::new(Mutex::new(None));

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let seen = request.clone();
        let handle = tokio::spawn(async move {
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
            *seen.lock().await = Some(String::from_utf8_lossy(&buffer).into_owned());
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            request,
            handle,
        }
    }

    /// The body the driver sent, parsed.
    async fn sent_body(&self) -> serde_json::Value {
        let raw = self
            .request
            .lock()
            .await
            .clone()
            .expect("a request arrived");
        let (_, body) = raw.split_once("\r\n\r\n").expect("headers then body");
        serde_json::from_str(body).expect("the driver sent JSON")
    }

    async fn sent_headers(&self) -> String {
        let raw = self
            .request
            .lock()
            .await
            .clone()
            .expect("a request arrived");
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

#[tokio::test]
async fn anthropic_completes_over_a_real_socket() {
    let server = FakeProvider::serving(
        200,
        "application/json",
        r#"{"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":4,"output_tokens":2}}"#,
    )
    .await;

    let response = AnthropicDriver::new()
        .complete(request(
            ModelSpec::anthropic("claude-x")
                .api_key("k")
                .base_url(&server.base_url),
        ))
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
    let response = AnthropicDriver::new()
        .complete_streaming(
            request(
                ModelSpec::anthropic("claude-x")
                    .api_key("k")
                    .base_url(&server.base_url),
            ),
            &mut sink,
        )
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

    let response = OpenAiDriver::new()
        .complete(request(
            ModelSpec::openai("gpt-x").base_url(&server.base_url),
        ))
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
async fn openai_streams_over_a_real_socket() {
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
    let response = OpenAiDriver::new()
        .complete_streaming(
            request(ModelSpec::openai("gpt-x").base_url(&server.base_url)),
            &mut sink,
        )
        .await
        .expect("the stream folds");

    assert_eq!(sink.0, vec!["He", "llo"]);
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.usage.output_tokens, 5);
    let body = server.sent_body().await;
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

/// An error status is classified rather than parsed — and the body, which may
/// echo the request, is reported as the provider's message, not swallowed.
#[tokio::test]
async fn an_http_error_is_classified_by_retryability() {
    let server = FakeProvider::serving(429, "application/json", r#"{"error":"slow down"}"#).await;

    let error = OpenAiDriver::new()
        .complete(request(
            ModelSpec::openai("gpt-x").base_url(&server.base_url),
        ))
        .await
        .expect_err("429 is an error");

    assert!(error.is_retryable(), "a rate limit is worth retrying");
    assert!(error.to_string().contains("429"), "{error}");
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

    let error = AnthropicDriver::new()
        .complete(request(
            ModelSpec::anthropic("claude-x")
                .api_key("k")
                .base_url(&server.base_url),
        ))
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
