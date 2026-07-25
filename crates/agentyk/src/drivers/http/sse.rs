//! Server-sent-events decoding, shared by every streaming HTTP driver.
//!
//! Providers differ in what their events *mean*, not in how the stream is
//! framed, so the framing lives here once. Two drivers previously carried
//! their own `drain_lines` + `parse_sse_data_line` pair — and, worse, their
//! streaming tests re-implemented the chunk loop in the test body, so the
//! loop that actually ran in production was never exercised. This type is
//! that loop, and it is tested directly.

/// Reassembles `data:` payloads from arbitrarily-chunked SSE bytes.
///
/// A chunk may end mid-line, so a partial line is held until the rest
/// arrives. Non-`data:` fields (`event:`, `id:`, comments) are skipped: every
/// provider's payload is self-describing enough to interpret on its own.
///
/// Payloads come out as raw strings. Decoding them is the accumulator's job,
/// because only the provider knows what shape to expect — and a payload that
/// fails to decode should be reported, not quietly dropped here.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of the response body; returns the `data:` payloads
    /// it completed, in order.
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut payloads = Vec::new();
        while let Some(newline) = self.buffer.find('\n') {
            let line = self.buffer[..newline].to_string();
            self.buffer.drain(..=newline);
            if let Some(data) = data_field(&line)
                && !data.is_empty()
            {
                payloads.push(data.to_string());
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

    #[test]
    fn yields_data_payloads_and_skips_everything_else() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(concat!(
            "event: message_start\n",
            "data: {\"a\":1}\n",
            ": a comment\n",
            "id: 7\n",
            "\n",
            "data:{\"b\":2}\n",
        ));
        assert_eq!(payloads, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn holds_a_partial_line_until_the_rest_arrives() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: {\"a\":").is_empty());
        assert!(decoder.push("1").is_empty());
        assert_eq!(decoder.push("}\n"), vec!["{\"a\":1}"]);
    }

    #[test]
    fn a_line_split_across_chunks_mid_field_still_decodes() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("dat").is_empty());
        assert_eq!(decoder.push("a: {\"x\":true}\n"), vec!["{\"x\":true}"]);
    }

    #[test]
    fn the_terminator_is_passed_through_for_the_accumulator_to_recognize() {
        // Not the decoder's call: `[DONE]` is one provider's convention.
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push("data: [DONE]\n"), vec!["[DONE]"]);
        assert!(decoder.push("data: \n").is_empty());
    }

    #[test]
    fn carriage_returns_are_tolerated() {
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push("data: {\"a\":1}\r\n"), vec!["{\"a\":1}"]);
    }
}
