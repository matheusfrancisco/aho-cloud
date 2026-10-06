//! Usage metering for the model gateway (server.md "Metering"): [`UsageTap`] reads the bytes the
//! gateway forwards; [`ObservedUsage`] turns into a domain `Usage` delta with the operator's prices.
//!
//! - Non-streaming: the body's `usage`.
//! - Streaming: `message_start.message.usage` and `message_delta.usage`. Streamed usage is
//!   cumulative, so the last value per field wins, never a sum.
//! - A stream without `message_delta` (client gone, upstream error): input is exact from
//!   `message_start`; output is estimated as `ceil(streamed delta text bytes / 4)` [heuristic].

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::value::RawValue;

use mage_core::domain::Usage;

use super::ModelPrice;
use crate::ports::llm::ModelId;

/// Longest SSE line the tap buffers; longer lines are skipped and the usage marked estimated.
pub const MAX_LINE: usize = 64 * 1024;

/// What one model call consumed, as observed by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedUsage {
    /// Model named by the response (Vertex may answer with its own id); pricing uses the
    /// requested model.
    pub model: Option<ModelId>,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    /// Output (or an unreadable line) was estimated rather than read from the final usage.
    pub estimated: bool,
    /// The response finished normally (full body, or `message_stop` without an `error` event).
    pub completed: bool,
}

impl ObservedUsage {
    /// Usage of a non-streaming 2xx Messages response.
    pub fn from_message(body: &[u8]) -> ObservedUsage {
        #[derive(Deserialize)]
        struct Message {
            model: Option<String>,
            usage: Option<WireUsage>,
        }
        match serde_json::from_slice::<Message>(body) {
            Ok(Message { model, usage: Some(usage) }) => ObservedUsage {
                model: model.map(ModelId),
                input_tokens: usage.input_tokens.unwrap_or(0),
                cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                estimated: usage.output_tokens.is_none(),
                completed: true,
            },
            Ok(Message { model, usage: None }) => ObservedUsage { model: model.map(ModelId), estimated: true, ..ObservedUsage::default() },
            Err(_) => ObservedUsage { estimated: true, ..ObservedUsage::default() },
        }
    }

    /// Domain usage delta: `input_tokens` sums the three input fields; `turns` = 1 if completed.
    pub fn usage(&self, price: &ModelPrice) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(self.cache_creation_input_tokens).saturating_add(self.cache_read_input_tokens),
            output_tokens: self.output_tokens,
            cost_micros: price.cost_micros(self),
            turns: u32::from(self.completed),
            wall: std::time::Duration::ZERO,
        }
    }
}

/// Usage fields of the Messages API (`usage` object). Absent and `null` read as `None`.
#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct StreamEvent<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    message: Option<StartMessage>,
    usage: Option<WireUsage>,
    #[serde(borrow)]
    delta: Option<Delta<'a>>,
}

#[derive(Deserialize)]
struct StartMessage {
    model: Option<String>,
    usage: Option<WireUsage>,
}

/// `content_block_delta.delta`: only the generated text matters (kept raw, no allocation).
#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(borrow)]
    text: Option<&'a RawValue>,
    #[serde(borrow)]
    partial_json: Option<&'a RawValue>,
    #[serde(borrow)]
    thinking: Option<&'a RawValue>,
}

/// Incremental SSE reader over the forwarded bytes. Holds only the current incomplete line
/// (at most [`MAX_LINE`]); the result does not depend on how the stream was chunked.
#[derive(Debug, Default)]
pub struct UsageTap {
    partial: Vec<u8>,
    /// The current line already exceeded [`MAX_LINE`]: drop bytes until its newline.
    discarding: bool,
    /// A line was skipped (too long or not JSON): the result is an estimate.
    unknown: bool,
    model: Option<String>,
    input: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
    output: Option<u64>,
    /// `output` came from `message_delta` (final).
    output_final: bool,
    delta_bytes: u64,
    stopped: bool,
    errored: bool,
}

impl UsageTap {
    pub fn new() -> UsageTap {
        UsageTap::default()
    }

    /// Reads one forwarded chunk.
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
            let (head, tail) = (&rest[..pos], &rest[pos + 1..]);
            if self.discarding {
                self.discarding = false;
            } else if self.partial.is_empty() {
                self.line(head);
            } else if self.partial.len() + head.len() > MAX_LINE {
                self.unknown = true;
                self.partial.clear();
            } else {
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(head);
                self.line(&line);
                line.clear();
                self.partial = line;
            }
            rest = tail;
        }
        if rest.is_empty() || self.discarding {
            return;
        }
        if self.partial.len() + rest.len() > MAX_LINE {
            self.unknown = true;
            self.discarding = true;
            self.partial.clear();
        } else {
            self.partial.extend_from_slice(rest);
        }
    }

    /// Usage so far; call when the stream settles (end, error or client disconnect).
    pub fn observe(&self) -> ObservedUsage {
        let (output_tokens, output_estimated) = if self.output_final {
            (self.output.unwrap_or(0), false)
        } else {
            (self.output.unwrap_or(0).max(self.delta_bytes.div_ceil(4)), true)
        };
        ObservedUsage {
            model: self.model.clone().map(ModelId),
            input_tokens: self.input.unwrap_or(0),
            cache_creation_input_tokens: self.cache_creation.unwrap_or(0),
            cache_read_input_tokens: self.cache_read.unwrap_or(0),
            output_tokens,
            estimated: self.unknown || output_estimated,
            completed: self.stopped && !self.errored,
        }
    }

    fn line(&mut self, line: &[u8]) {
        if line.len() > MAX_LINE {
            self.unknown = true;
            return;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        match serde_json::from_slice::<StreamEvent<'_>>(data) {
            Ok(event) => self.event(event),
            Err(_) => self.unknown = true,
        }
    }

    fn event(&mut self, event: StreamEvent<'_>) {
        match event.kind.as_ref() {
            "message_start" => {
                if let Some(message) = event.message {
                    if message.model.is_some() {
                        self.model = message.model;
                    }
                    if let Some(usage) = message.usage {
                        self.merge(usage, false);
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = event.usage {
                    self.merge(usage, true);
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    for raw in [delta.text, delta.partial_json, delta.thinking].into_iter().flatten() {
                        self.delta_bytes = self.delta_bytes.saturating_add(json_string_len(raw) as u64);
                    }
                }
            }
            "message_stop" => self.stopped = true,
            "error" => self.errored = true,
            _ => {}
        }
    }

    /// Last value per field wins (streamed usage is cumulative).
    fn merge(&mut self, usage: WireUsage, from_delta: bool) {
        if let Some(v) = usage.input_tokens {
            self.input = Some(v);
        }
        if let Some(v) = usage.cache_creation_input_tokens {
            self.cache_creation = Some(v);
        }
        if let Some(v) = usage.cache_read_input_tokens {
            self.cache_read = Some(v);
        }
        if let Some(v) = usage.output_tokens {
            self.output = Some(v);
            self.output_final |= from_delta;
        }
    }
}

/// Decoded byte length of a raw JSON string; decodes only when it holds escapes.
fn json_string_len(raw: &RawValue) -> usize {
    let quoted = raw.get();
    if !quoted.contains('\\') {
        return quoted.len().saturating_sub(2);
    }
    serde_json::from_str::<String>(quoted).map_or(quoted.len().saturating_sub(2), |s| s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        // USD 3 / 15 / 3.75 / 0.30 per million tokens, in micros per million.
        ModelPrice { input: 3_000_000, output: 15_000_000, cache_write: 3_750_000, cache_read: 300_000 }
    }

    /// A recorded-shape Anthropic stream: usage in `message_start`, cumulative repeat of the input
    /// fields plus the final output in `message_delta`, pings, a tool-use block.
    const FULL_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":2000,\"output_tokens\":1}}}\n",
        "\n",
        "event: ping\n",
        "data: {\"type\": \"ping\"}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \\\"wor\u{00e9}ld\\\"\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"mcp__mage_knowledge__search\",\"input\":{}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\": \\\"x\\\"}\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":2000,\"output_tokens\":57}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    fn full_expected() -> ObservedUsage {
        ObservedUsage {
            model: Some(ModelId("claude-sonnet-4-5".into())),
            input_tokens: 120,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 2000,
            output_tokens: 57,
            estimated: false,
            completed: true,
        }
    }

    fn tap_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> ObservedUsage {
        let mut tap = UsageTap::new();
        for chunk in chunks {
            tap.feed(chunk);
        }
        tap.observe()
    }

    #[test]
    fn stream_usage_is_identical_for_every_split_point() {
        let bytes = FULL_STREAM.as_bytes();
        assert_eq!(tap_chunks([bytes]), full_expected());
        for split in 0..=bytes.len() {
            let (a, b) = bytes.split_at(split);
            assert_eq!(tap_chunks([a, b]), full_expected(), "split at {split}");
        }
        assert_eq!(tap_chunks(bytes.chunks(1)), full_expected(), "byte by byte");
        assert_eq!(tap_chunks(bytes.chunks(7)), full_expected(), "7-byte chunks");
    }

    #[test]
    fn crlf_line_endings_are_read() {
        let crlf = FULL_STREAM.replace('\n', "\r\n");
        assert_eq!(tap_chunks([crlf.as_bytes()]), full_expected());
    }

    #[test]
    fn cumulative_message_delta_usage_is_not_double_counted() {
        // Two message_delta events (a proxy may repeat it): the last value wins per field.
        let stream = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":10,\"output_tokens\":25}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let observed = tap_chunks([stream.as_bytes()]);
        assert_eq!((observed.input_tokens, observed.output_tokens, observed.estimated), (10, 25, false));
    }

    #[test]
    fn missing_message_delta_estimates_output_from_streamed_text() {
        // Client disconnected after 10 text bytes + 5 partial_json bytes: input exact, output
        // ceil(15 / 4) = 4, estimated, not completed.
        let stream = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":99,\"cache_read_input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"0123456789\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
            "data: {\"type\":\"content_bl",
        );
        let observed = tap_chunks([stream.as_bytes()]);
        assert_eq!(observed.input_tokens, 99);
        assert_eq!(observed.cache_read_input_tokens, 1);
        assert_eq!(observed.output_tokens, 4);
        assert!(observed.estimated);
        assert!(!observed.completed);
        assert_eq!(observed.usage(&price()).turns, 0);
    }

    #[test]
    fn error_event_mid_stream_is_not_completed() {
        let stream = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let observed = tap_chunks([stream.as_bytes()]);
        assert_eq!(observed.input_tokens, 5);
        assert!(!observed.completed);
        assert!(observed.estimated);
    }

    #[test]
    fn overlong_line_marks_usage_estimated_and_is_skipped_for_any_chunking() {
        let huge = format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
            "x".repeat(MAX_LINE)
        );
        let stream = format!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"model\":\"m\",\"usage\":{{\"input_tokens\":7,\"output_tokens\":1}}}}}}\n\n{huge}\
             data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":40000}}}}\n\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let whole = tap_chunks([stream.as_bytes()]);
        let small = tap_chunks(stream.as_bytes().chunks(1000));
        assert_eq!(whole, small);
        assert_eq!((whole.input_tokens, whole.output_tokens), (7, 40000));
        assert!(whole.estimated, "a skipped line makes the result an estimate");
        assert!(whole.completed);
    }

    #[test]
    fn line_of_exactly_max_len_is_read() {
        let prefix =
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}},\"pad\":\"";
        let suffix = "\"}";
        let pad = "p".repeat(MAX_LINE - prefix.len() - suffix.len());
        let line = format!("{prefix}{pad}{suffix}");
        assert_eq!(line.len(), MAX_LINE);
        let mut tap = UsageTap::new();
        for chunk in format!("{line}\n\n").as_bytes().chunks(4096) {
            tap.feed(chunk);
        }
        assert!(!tap.unknown, "a line of exactly MAX_LINE bytes is read");
        assert_eq!(tap.observe().input_tokens, 3);
        let mut tap = UsageTap::new();
        tap.feed(format!("{line}x\n").as_bytes());
        assert!(tap.unknown && tap.observe().estimated, "one byte longer is skipped");
        assert_eq!(tap.observe().input_tokens, 0);
    }

    #[test]
    fn non_streaming_body_usage() {
        let body = br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":11,"cache_creation_input_tokens":null,"cache_read_input_tokens":4,"output_tokens":9}}"#;
        let observed = ObservedUsage::from_message(body);
        assert_eq!(
            observed,
            ObservedUsage {
                model: Some(ModelId("claude-sonnet-4-5".into())),
                input_tokens: 11,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 4,
                output_tokens: 9,
                estimated: false,
                completed: true,
            }
        );
        assert!(ObservedUsage::from_message(b"not json").estimated);
    }

    #[test]
    fn usage_delta_prices_each_field_and_rounds_cost_up() {
        let observed = full_expected();
        let usage = observed.usage(&price());
        assert_eq!(usage.input_tokens, 120 + 30 + 2000);
        assert_eq!(usage.output_tokens, 57);
        assert_eq!(usage.turns, 1);
        // 120*3 + 30*3.75 + 2000*0.30 + 57*15 = 360 + 112.5 + 600 + 855 = 1927.5 micros → 1928.
        assert_eq!(usage.cost_micros, 1928);
        let one = ObservedUsage { output_tokens: 1, ..ObservedUsage::default() };
        assert_eq!(one.usage(&ModelPrice { input: 0, output: 1, cache_write: 0, cache_read: 0 }).cost_micros, 1, "fractions round up");
        assert_eq!(ObservedUsage::default().usage(&price()), Usage::default());
    }
}
