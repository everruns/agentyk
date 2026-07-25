//! The shared HTTP driver machinery.
//!
//! A provider driver should be nothing but wire mapping: build a request body,
//! read a response, fold a stream. Everything around that — sending, status
//! classification, transport-error classification, SSE framing, forwarding
//! deltas — is identical across providers, and was previously duplicated per
//! driver, where it drifts.
//!
//! A driver implements [`HttpProvider`] plus a [`StreamAccumulator`], and its
//! [`ChatDriver`](agentyk_core::driver::ChatDriver) impl delegates to
//! [`complete`] and [`complete_streaming`]. Adding a provider is the wire
//! mapping and nothing else.

pub(crate) mod sse;

use agentyk_core::driver::{ChatRequest, ChatResponse, DeltaSink, ModelSpec};
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
pub(crate) trait HttpProvider: Send + Sync {
    type Accumulator: StreamAccumulator;

    /// Short name used in error messages, e.g. `"anthropic"`.
    fn label(&self) -> &str;

    /// Used when [`ModelSpec::base_url`] is unset.
    fn default_base_url(&self) -> &str;

    /// Path appended to the base url, e.g. `"/v1/messages"`.
    fn endpoint(&self) -> &str;

    /// Attach credentials and provider headers. Returning an error here is
    /// how a provider that *requires* a key reports a missing one.
    fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        model: &ModelSpec,
    ) -> Result<reqwest::RequestBuilder>;

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

fn endpoint_url<P: HttpProvider>(provider: &P, model: &ModelSpec) -> String {
    let base = model
        .base_url
        .as_deref()
        .unwrap_or_else(|| provider.default_base_url());
    format!("{}{}", base.trim_end_matches('/'), provider.endpoint())
}

/// Send a prepared body and hand back a successful response, or a classified
/// error. The single place a provider's HTTP failure becomes an [`Error`].
async fn send<P: HttpProvider>(
    provider: &P,
    client: &reqwest::Client,
    model: &ModelSpec,
    body: &Value,
) -> Result<reqwest::Response> {
    let builder = client.post(endpoint_url(provider, model)).json(body);
    let response = provider
        .authorize(builder, model)?
        .send()
        .await
        .map_err(|e| network_error(provider.label(), "request failed", &e))?;

    let status = response.status();
    if !status.is_success() {
        let payload: Value = response.json().await.unwrap_or(Value::Null);
        return Err(Error::driver(
            provider.classify_status(status),
            format!("{} http {status}: {payload}", provider.label()),
        ));
    }
    Ok(response)
}

/// One non-streaming completion.
pub(crate) async fn complete<P: HttpProvider>(
    provider: &P,
    client: &reqwest::Client,
    request: ChatRequest,
) -> Result<ChatResponse> {
    let body = provider.build_body(&request);
    let response = send(provider, client, &request.model, &body).await?;
    let text = response
        .text()
        .await
        .map_err(|e| network_error(provider.label(), "response body could not be read", &e))?;
    provider.parse_response(&text)
}

/// One streaming completion: forwards text increments to `sink` as they
/// arrive and returns the same response [`complete`] would.
pub(crate) async fn complete_streaming<P: HttpProvider>(
    provider: &P,
    client: &reqwest::Client,
    request: ChatRequest,
    sink: &mut dyn DeltaSink,
) -> Result<ChatResponse> {
    let mut body = provider.build_body(&request);
    provider.enable_streaming(&mut body);

    let response = send(provider, client, &request.model, &body).await?;
    let mut bytes = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut accumulator = P::Accumulator::default();

    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| network_error(provider.label(), "stream read failed", &e))?;
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
