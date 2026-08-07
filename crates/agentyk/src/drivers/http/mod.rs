//! The shared HTTP driver machinery.
//!
//! A driver should be nothing but wire mapping: build a request body, read a
//! response, fold a stream. Everything around that — sending, status
//! classification, transport-error classification, SSE framing, forwarding
//! deltas — is identical across protocols, and was previously duplicated per
//! driver, where it drifts.
//!
//! Where the request goes and what credential it carries is not here either:
//! that is the [`Provider`](agentyk_core::provider::Provider)'s, resolved per
//! request and read off [`ChatRequest::endpoint`].
//!
//! A driver implements [`WireProtocol`] plus a [`StreamAccumulator`], and its
//! [`ChatDriver`](agentyk_core::driver::ChatDriver) impl delegates to
//! [`complete`] and [`complete_streaming`]. Adding a provider is the wire
//! mapping and nothing else.

pub(crate) mod sse;

use agentyk_core::driver::{ChatRequest, ChatResponse, DeltaSink};
use agentyk_core::error::{Error, LlmErrorKind, Result};
use futures_util::StreamExt;
use serde_json::Value;

use sse::SseDecoder;

/// Folds a provider's streaming events into the same [`ChatResponse`] its
/// non-streaming endpoint would return.
///
/// Implementations deserialize `data` into their own typed event enum rather
/// than indexing a `Value`, so a renamed field is a parse failure with a
/// field name and position in it, not silently-missing text.
pub(crate) trait StreamAccumulator: Default {
    /// Apply one SSE `data:` payload. Returns the text increment to forward
    /// to the [`DeltaSink`], if this event produced one — reasoning/thinking
    /// deltas and bookkeeping events return `None`.
    ///
    /// Returns `Err` only for a payload this provider should have
    /// understood; unknown *event types* are expected (providers add them) and
    /// must deserialize to an ignored variant instead.
    fn apply(&mut self, data: &str) -> Result<Option<String>>;

    /// Whether this payload marks the end of the stream rather than an event
    /// — OpenAI's `[DONE]`, which is not JSON and must not be parsed as one.
    fn is_terminator(_data: &str) -> bool {
        false
    }

    /// The answer text accumulated so far, for the sink's `accumulated`
    /// argument.
    fn text(&self) -> &str;

    fn finish(self) -> ChatResponse;
}

/// Deserialize one wire payload, turning a decode failure into a driver error
/// that names the provider. Serde's message carries the offending field and
/// position, which is the whole point of typing these.
/// The HTTP client the bundled drivers use by default.
///
/// Trusts **both** the bundled public roots and the machine's own trust store.
/// That second half is not optional in practice: plenty of environments —
/// corporate networks, CI sandboxes, anything with an inspecting egress proxy
/// — terminate TLS with a private CA installed on the machine. A client that
/// only knows Mozilla's roots cannot reach *any* provider from inside one, and
/// the failure it reports is an unhelpful "error sending request".
///
/// Found exactly that way: a live yolop run could not reach Anthropic through
/// an intercepting proxy while `curl` on the same box could, because `curl`
/// reads the system store and this client did not.
///
/// Falls back to a default client if the trust store cannot be read, so a
/// minimal container with no CA bundle keeps working on the bundled roots
/// rather than failing to construct.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .build()
        .unwrap_or_default()
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(label: &str, body: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        Error::driver(
            LlmErrorKind::Unknown,
            // Deliberately no body echo: it holds the conversation.
            format!("{label} response did not match the expected shape: {e}"),
        )
    })
}

/// One provider's wire protocol.
pub(crate) trait WireProtocol: Send + Sync {
    type Accumulator: StreamAccumulator;

    /// Short name for the protocol, used when a *wire* payload is wrong —
    /// e.g. `"anthropic messages"`. Failures that are about the service
    /// rather than the format name the provider instead.
    fn label(&self) -> &str;

    /// Path appended to the provider's base url, e.g. `"/v1/messages"`.
    fn endpoint(&self) -> &str;

    /// Headers the *protocol* requires on every request, whichever service
    /// serves it — Anthropic's `anthropic-version`. Provider headers are
    /// applied after these, so a service can override one it pins itself.
    fn protocol_headers(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    fn build_body(&self, request: &ChatRequest) -> Value;

    /// Turn on streaming in a body from [`Self::build_body`].
    fn enable_streaming(&self, body: &mut Value);

    /// Read a non-streaming response body. Returns an error when the body
    /// does not match the shape this driver depends on — the alternative is
    /// handing the model an empty message and no way to tell why.
    fn parse_response(&self, body: &str) -> Result<ChatResponse>;

    /// Map an HTTP status to a retryability class. The default covers the
    /// common codes; override to add provider-specific ones (Anthropic's 529).
    fn classify_status(&self, status: reqwest::StatusCode) -> LlmErrorKind {
        classify_status(status)
    }
}

/// The status classification every provider starts from — see
/// [`LlmErrorKind`] for what each class means for retrying.
pub(crate) fn classify_status(status: reqwest::StatusCode) -> LlmErrorKind {
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

/// Classify a transport-level failure — the request never got an HTTP
/// response at all, so there is no status to read.
fn network_error(label: &str, context: &str, error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        LlmErrorKind::Timeout
    } else {
        LlmErrorKind::Network
    };
    Error::driver(kind, format!("{label} {context}: {error}"))
}

/// Where this request goes: the provider's base url plus the protocol's path.
///
/// A missing base url is a wiring mistake, not a default to paper over — the
/// protocol has no opinion about which service speaks it, so guessing one
/// would send a gateway's traffic to a vendor.
fn endpoint_url<P: WireProtocol>(protocol: &P, request: &ChatRequest) -> Result<String> {
    let base = request.endpoint.base_url.as_deref().ok_or_else(|| {
        Error::driver(
            LlmErrorKind::InvalidRequest,
            format!(
                "provider `{}` has no base url, which the {} protocol needs — set it with Provider::base_url",
                request.model.provider,
                protocol.label()
            ),
        )
    })?;
    Ok(format!("{base}{}", protocol.endpoint()))
}

/// Send a prepared body and hand back a successful response, or a classified
/// error. The single place a provider's HTTP failure becomes an [`Error`].
async fn send<P: WireProtocol>(
    protocol: &P,
    client: &reqwest::Client,
    request: &ChatRequest,
    body: &Value,
) -> Result<reqwest::Response> {
    // Errors here name the *provider*: a 402 from OpenRouter reported as
    // "openai" sends whoever reads the log to the wrong dashboard.
    let service = request.model.provider.to_string();
    let mut builder = client.post(endpoint_url(protocol, request)?).json(body);
    for (name, value) in protocol.protocol_headers() {
        builder = builder.header(name, value);
    }
    for (name, value) in &request.endpoint.headers {
        builder = builder.header(name, value);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| network_error(&service, "request failed", &e))?;

    let status = response.status();
    if !status.is_success() {
        let payload: Value = response.json().await.unwrap_or(Value::Null);
        return Err(Error::driver(
            protocol.classify_status(status),
            format!("{service} http {status}: {payload}"),
        ));
    }
    Ok(response)
}

/// One non-streaming completion.
pub(crate) async fn complete<P: WireProtocol>(
    protocol: &P,
    client: &reqwest::Client,
    request: ChatRequest,
) -> Result<ChatResponse> {
    let body = protocol.build_body(&request);
    let response = send(protocol, client, &request, &body).await?;
    let text = response.text().await.map_err(|e| {
        network_error(
            &request.model.provider.to_string(),
            "response body could not be read",
            &e,
        )
    })?;
    protocol.parse_response(&text)
}

/// One streaming completion: forwards text increments to `sink` as they
/// arrive and returns the same response [`complete`] would.
pub(crate) async fn complete_streaming<P: WireProtocol>(
    protocol: &P,
    client: &reqwest::Client,
    request: ChatRequest,
    sink: &mut dyn DeltaSink,
) -> Result<ChatResponse> {
    let mut body = protocol.build_body(&request);
    protocol.enable_streaming(&mut body);

    let service = request.model.provider.to_string();
    let response = send(protocol, client, &request, &body).await?;
    let mut bytes = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut accumulator = P::Accumulator::default();

    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| network_error(&service, "stream read failed", &e))?;
        pump(
            &mut accumulator,
            &mut decoder,
            &String::from_utf8_lossy(&chunk),
            sink,
        )
        .await?;
    }

    Ok(accumulator.finish())
}

/// One chunk of a streaming body: decode, apply, forward. Factored out so
/// tests drive the *same* loop `complete_streaming` runs — the previous
/// per-driver tests re-implemented it, which meant the production loop was
/// never covered.
async fn pump<A: StreamAccumulator>(
    accumulator: &mut A,
    decoder: &mut SseDecoder,
    chunk: &str,
    sink: &mut dyn DeltaSink,
) -> Result<()> {
    for payload in decoder.push(chunk) {
        if A::is_terminator(&payload) {
            continue;
        }
        if let Some(delta) = accumulator.apply(&payload)? {
            sink.delta(&delta, accumulator.text()).await?;
        }
    }
    Ok(())
}

/// Run an accumulator over canned body chunks through the real streaming
/// loop. Chunk boundaries are arbitrary, so tests can split a body anywhere
/// to prove reassembly.
#[cfg(test)]
pub(crate) async fn drive_stream<A: StreamAccumulator>(
    chunks: &[&str],
    sink: &mut dyn DeltaSink,
) -> Result<ChatResponse> {
    let mut decoder = SseDecoder::new();
    let mut accumulator = A::default();
    for chunk in chunks {
        pump(&mut accumulator, &mut decoder, chunk, sink).await?;
    }
    Ok(accumulator.finish())
}

/// Collects the deltas a driver reports, for streaming tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingSink {
    pub(crate) deltas: Vec<String>,
    pub(crate) accumulated: Vec<String>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl DeltaSink for RecordingSink {
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()> {
        self.deltas.push(delta.to_string());
        self.accumulated.push(accumulated.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk_core::driver::ModelSpec;
    use agentyk_core::message::Message;

    struct TestProtocol;

    impl WireProtocol for TestProtocol {
        type Accumulator = crate::drivers::openai::OpenAiStream;

        fn label(&self) -> &str {
            "test"
        }

        fn endpoint(&self) -> &str {
            "/v1/chat"
        }

        fn build_body(&self, _request: &ChatRequest) -> Value {
            Value::Null
        }

        fn enable_streaming(&self, _body: &mut Value) {}

        fn parse_response(&self, _body: &str) -> Result<ChatResponse> {
            unreachable!()
        }
    }

    fn request(base_url: Option<&str>) -> ChatRequest {
        let mut request = ChatRequest::new(
            ModelSpec::on("some-gateway", "m"),
            vec![Message::user("hi")],
        );
        request.endpoint.base_url = base_url.map(str::to_string);
        request
    }

    #[test]
    fn the_url_is_the_providers_base_plus_the_protocols_path() {
        assert_eq!(
            endpoint_url(&TestProtocol, &request(Some("https://gateway.internal"))).unwrap(),
            "https://gateway.internal/v1/chat"
        );
    }

    #[test]
    fn a_provider_with_no_base_url_fails_naming_itself() {
        // The protocol has no default host to fall back on — guessing one
        // would send a gateway\'s traffic to a vendor.
        let error = endpoint_url(&TestProtocol, &request(None)).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("some-gateway"), "{message}");
        assert!(message.contains("Provider::base_url"), "{message}");
    }

    #[test]
    fn the_default_classification_splits_retryable_from_terminal() {
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
}
