//! Server-sent-events decoding, shared by every streaming HTTP driver.
//!
//! Providers differ in what their events *mean*, not in how the stream is
//! framed, so the framing lives here once. Two drivers previously carried
//! their own `drain_lines` + `parse_sse_data_line` pair — and, worse, their
//! streaming tests re-implemented the chunk loop in the test body, so the
//! loop that actually ran in production was never exercised. This type is
//! that loop, and it is tested directly.

use serde_json::Value;

/// Reassembles `data:` payloads from arbitrarily-chunked SSE bytes.
///
/// A chunk may end mid-line, so a partial line is held until the rest
/// arrives. Non-`data:` fields (`event:`, `id:`, comments) and payloads that
/// are not JSON — including OpenAI's `[DONE]` sentinel — are skipped: every
/// provider's payload is self-describing enough to interpret on its own.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of the response body; returns the decoded
    /// payloads it completed, in order.
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.buffer.push_str(chunk);
        let mut payloads = Vec::new();
        while let Some(newline) = self.buffer.find('\n') {
            let line = self.buffer[..newline].to_string();
            self.buffer.drain(..=newline);
            if let Some(data) = data_field(&line)
                && let Ok(payload) = serde_json::from_str::<Value>(data)
            {
                payloads.push(payload);
            }
        }
        payloads
    }
}

/// The payload of one `data:` line, or `None` for any other line.
fn data_field(line: &str) -> Option<&str> {
    line.trim_end_matches('\r')
        .strip_prefix("data:")
        .map(str::trim_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_data_lines_and_skips_everything_else() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(concat!(
            "event: message_start\n",
            "data: {\"a\":1}\n",
            ": a comment\n",
            "id: 7\n",
            "\n",
            "data:{\"b\":2}\n",
        ));
        assert_eq!(payloads, vec![json!({"a": 1}), json!({"b": 2})]);
    }

    #[test]
    fn holds_a_partial_line_until_the_rest_arrives() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: {\"a\":").is_empty());
        assert!(decoder.push("1").is_empty());
        assert_eq!(decoder.push("}\n"), vec![json!({"a": 1})]);
    }

    #[test]
    fn a_line_split_across_chunks_mid_field_still_decodes() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("dat").is_empty());
        assert_eq!(decoder.push("a: {\"x\":true}\n"), vec![json!({"x": true})]);
    }

    #[test]
    fn non_json_payloads_are_skipped() {
        // OpenAI's terminator needs no special case: it simply isn't JSON.
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: [DONE]\n").is_empty());
        assert!(decoder.push("data: \n").is_empty());
    }

    #[test]
    fn carriage_returns_are_tolerated() {
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push("data: {\"a\":1}\r\n"), vec![json!({"a": 1})]);
    }
}
