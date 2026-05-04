use crate::error::ApiError;
use crate::types::StreamEvent;
use runtime::agent_debug_log;

const SSE_EVENT_PING: &str = "ping";
const SSE_DONE_MARKER: &str = "[DONE]";
const SSE_STAGE_PARSE: &str = "parse";
const SSE_STAGE_DONE: &str = "done";
const SSE_STAGE_NO_DATA: &str = "no_data";

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    provider: Option<String>,
    model: Option<String>,
    trace_id: Option<String>,
    chunk_seq: u64,
    frame_seq: u64,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the provider name and model to this parser so that JSON
    /// deserialization failures within streamed frames carry enough context
    /// for callers to understand which upstream produced the unparseable
    /// payload.
    #[must_use]
    pub fn with_context(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ApiError> {
        self.chunk_seq += 1;
        let buffer_before = self.buffer.len();
        self.buffer.extend_from_slice(chunk);
        agent_debug_log(
            "sse.chunk",
            format!(
                "provider={}\nmodel={}\ntrace_id={}\nchunk_seq={}\nchunk_bytes={}\nbuffer_bytes_before={}\nbuffer_bytes_after={}\n{}",
                self.provider.as_deref().unwrap_or("unknown"),
                self.model.as_deref().unwrap_or("unknown"),
                self.trace_id.as_deref().unwrap_or("unknown"),
                self.chunk_seq,
                chunk.len(),
                buffer_before,
                self.buffer.len(),
                debug_bytes_summary("chunk", chunk, 2000),
            ),
        );
        let mut events = Vec::new();

        while let Some(frame) = self.next_frame() {
            self.frame_seq += 1;
            if let Some(event) = self.parse_frame_with_context(&frame, self.frame_seq)? {
                events.push(event);
            }
        }

        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ApiError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }

        let trailing = std::mem::take(&mut self.buffer);
        agent_debug_log(
            "sse.finish_trailing_buffer",
            format!(
                "provider={}\nmodel={}\ntrace_id={}\ntrailing_bytes={}\n{}",
                self.provider.as_deref().unwrap_or("unknown"),
                self.model.as_deref().unwrap_or("unknown"),
                self.trace_id.as_deref().unwrap_or("unknown"),
                trailing.len(),
                debug_bytes_summary("trailing", &trailing, 2000),
            ),
        );
        self.frame_seq += 1;
        match self.parse_frame_with_context(&String::from_utf8_lossy(&trailing), self.frame_seq)? {
            Some(event) => Ok(vec![event]),
            None => Ok(Vec::new()),
        }
    }

    fn parse_frame_with_context(
        &self,
        frame: &str,
        frame_seq: u64,
    ) -> Result<Option<StreamEvent>, ApiError> {
        let provider = self.provider.as_deref().unwrap_or("unknown");
        let model = self.model.as_deref().unwrap_or("unknown");
        parse_frame_with_provider_and_trace(
            frame,
            provider,
            model,
            self.trace_id.as_deref(),
            Some(frame_seq),
        )
    }

    fn next_frame(&mut self) -> Option<String> {
        let separator = self
            .buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|position| (position, 2))
            .or_else(|| {
                self.buffer
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| (position, 4))
            })?;

        let (position, separator_len) = separator;
        let frame = self
            .buffer
            .drain(..position + separator_len)
            .collect::<Vec<_>>();
        let frame_len = frame.len().saturating_sub(separator_len);
        Some(String::from_utf8_lossy(&frame[..frame_len]).into_owned())
    }
}

pub fn parse_frame(frame: &str) -> Result<Option<StreamEvent>, ApiError> {
    parse_frame_with_provider(frame, "unknown", "unknown")
}

pub(crate) fn parse_frame_with_provider(
    frame: &str,
    provider: &str,
    model: &str,
) -> Result<Option<StreamEvent>, ApiError> {
    parse_frame_with_provider_and_trace(frame, provider, model, None, None)
}

fn parse_frame_with_provider_and_trace(
    frame: &str,
    provider: &str,
    model: &str,
    trace_id: Option<&str>,
    frame_seq: Option<u64>,
) -> Result<Option<StreamEvent>, ApiError> {
    let log_context = SseFrameLogContext {
        provider,
        model,
        trace_id,
        frame_seq,
    };
    let trimmed = frame.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut data_lines = Vec::new();
    let mut event_name: Option<&str> = None;

    for line in trimmed.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            event_name = Some(name.trim());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }

    if matches!(event_name, Some(SSE_EVENT_PING)) {
        log_sse_frame_ignored(log_context, event_name, frame, SSE_EVENT_PING);
        return Ok(None);
    }

    if data_lines.is_empty() {
        log_sse_frame_ignored(log_context, event_name, frame, SSE_STAGE_NO_DATA);
        return Ok(None);
    }

    let payload = data_lines.join("\n");
    if payload == SSE_DONE_MARKER {
        log_sse_frame_payload(
            log_context,
            event_name,
            frame,
            &payload,
            data_lines.len(),
            SSE_STAGE_DONE,
        );
        return Ok(None);
    }

    log_sse_frame_payload(
        log_context,
        event_name,
        frame,
        &payload,
        data_lines.len(),
        SSE_STAGE_PARSE,
    );
    match serde_json::from_str::<StreamEvent>(&payload) {
        Ok(event) => Ok(Some(event)),
        Err(error) => {
            agent_debug_log(
                "sse.parse_error",
                format!(
                    "provider={provider}\nmodel={model}\nevent={}\npayload_bytes={}\npayload_chars={}\npayload_prefix={}\npayload_suffix={}\nerror={error}",
                    event_name.unwrap_or("unknown"),
                    payload.len(),
                    payload.chars().count(),
                    json_debug_string(&payload, 500),
                    json_debug_suffix(&payload, 500),
                ),
            );
            Err(ApiError::json_deserialize(provider, model, &payload, error))
        }
    }
}

#[derive(Clone, Copy)]
struct SseFrameLogContext<'a> {
    provider: &'a str,
    model: &'a str,
    trace_id: Option<&'a str>,
    frame_seq: Option<u64>,
}

impl SseFrameLogContext<'_> {
    fn trace_id_text(&self) -> &str {
        self.trace_id.unwrap_or("unknown")
    }

    fn frame_seq_text(&self) -> String {
        self.frame_seq
            .map_or_else(|| "unknown".to_string(), |value| value.to_string())
    }
}

fn log_sse_frame_ignored(
    context: SseFrameLogContext<'_>,
    event_name: Option<&str>,
    frame: &str,
    reason: &str,
) {
    agent_debug_log(
        "sse.frame_ignored",
        format!(
            "provider={provider}\nmodel={model}\ntrace_id={}\nframe_seq={}\nevent={}\nreason={reason}\n{}",
            context.trace_id_text(),
            context.frame_seq_text(),
            event_name.unwrap_or("unknown"),
            debug_text_summary("frame", frame, 2000),
            provider = context.provider,
            model = context.model,
        ),
    );
}

fn log_sse_frame_payload(
    context: SseFrameLogContext<'_>,
    event_name: Option<&str>,
    frame: &str,
    payload: &str,
    data_line_count: usize,
    stage: &str,
) {
    if !should_log_sse_frame_payload(stage) {
        return;
    }

    agent_debug_log(
        "sse.frame",
        format!(
            "provider={provider}\nmodel={model}\ntrace_id={}\nframe_seq={}\nevent={}\nstage={stage}\ndata_lines={data_line_count}\n{}\n{}",
            context.trace_id_text(),
            context.frame_seq_text(),
            event_name.unwrap_or("unknown"),
            debug_text_summary("frame", frame, 2000),
            debug_text_summary("payload", payload, 4000),
            provider = context.provider,
            model = context.model,
        ),
    );
}

fn should_log_sse_frame_payload(stage: &str) -> bool {
    stage != "parse"
}

fn debug_bytes_summary(label: &str, input: &[u8], limit: usize) -> String {
    debug_text_summary(label, &String::from_utf8_lossy(input), limit)
}

fn debug_text_summary(label: &str, input: &str, limit: usize) -> String {
    format!(
        "{label}_bytes={}\n{label}_chars={}\n{label}_full_available={}\n{label}_prefix={}\n{label}_suffix={}",
        input.len(),
        input.chars().count(),
        input.chars().count() <= limit,
        json_debug_string(input, limit),
        json_debug_suffix(input, limit)
    )
}

fn json_debug_string(input: &str, limit: usize) -> String {
    let value = input.chars().take(limit).collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

fn json_debug_suffix(input: &str, limit: usize) -> String {
    let mut suffix = input.chars().rev().take(limit).collect::<Vec<_>>();
    suffix.reverse();
    let value = suffix.into_iter().collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::{json_debug_suffix, parse_frame, should_log_sse_frame_payload, SseParser};
    use crate::types::{ContentBlockDelta, MessageDelta, OutputContentBlock, StreamEvent, Usage};

    #[test]
    fn parses_single_frame() {
        let frame = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hi\"}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockStart(
                crate::types::ContentBlockStartEvent {
                    index: 0,
                    content_block: OutputContentBlock::Text {
                        text: "Hi".to_string(),
                    },
                },
            ))
        );
    }

    #[test]
    fn parses_chunked_stream() {
        let mut parser = SseParser::new();
        let first = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel";
        let second = b"lo\"}}\n\n";

        assert!(parser
            .push(first)
            .expect("first chunk should buffer")
            .is_empty());
        let events = parser.push(second).expect("second chunk should parse");

        assert_eq!(
            events,
            vec![StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::TextDelta {
                        text: "Hello".to_string(),
                    },
                }
            )]
        );
    }

    #[test]
    fn ignores_ping_and_done() {
        let mut parser = SseParser::new();
        let payload = concat!(
            ": keepalive\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n"
        );

        let events = parser
            .push(payload.as_bytes())
            .expect("parser should succeed");
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageDelta(crate::types::MessageDeltaEvent {
                    delta: MessageDelta {
                        stop_reason: Some("tool_use".to_string()),
                        stop_sequence: None,
                    },
                    usage: Usage {
                        input_tokens: 1,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens: 2,
                        cache_creation: std::collections::BTreeMap::new(),
                    },
                }),
                StreamEvent::MessageStop(crate::types::MessageStopEvent {}),
            ]
        );
    }

    #[test]
    fn ignores_data_less_event_frames() {
        let frame = "event: ping\n\n";
        let event = parse_frame(frame).expect("frame without data should be ignored");
        assert_eq!(event, None);
    }

    #[test]
    fn parses_split_json_across_data_lines() {
        let frame = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::TextDelta {
                        text: "Hello".to_string(),
                    },
                }
            ))
        );
    }

    #[test]
    fn parses_thinking_content_block_start() {
        let frame = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":null}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockStart(
                crate::types::ContentBlockStartEvent {
                    index: 0,
                    content_block: OutputContentBlock::Thinking {
                        thinking: String::new(),
                        signature: None,
                    },
                },
            ))
        );
    }

    #[test]
    fn parses_thinking_related_deltas() {
        let thinking = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step 1\"}}\n\n"
        );
        let signature = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_123\"}}\n\n"
        );

        let thinking_event = parse_frame(thinking).expect("thinking delta should parse");
        let signature_event = parse_frame(signature).expect("signature delta should parse");

        assert_eq!(
            thinking_event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::ThinkingDelta {
                        thinking: "step 1".to_string(),
                    },
                }
            ))
        );
        assert_eq!(
            signature_event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::SignatureDelta {
                        signature: "sig_123".to_string(),
                    },
                }
            ))
        );
    }

    #[test]
    fn given_message_delta_frame_with_empty_usage_when_parsed_then_usage_defaults_to_zero() {
        // given
        let frame = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{}}\n\n"
        );

        // when
        let event = parse_frame(frame).expect("frame should parse");

        // then
        assert_eq!(
            event,
            Some(StreamEvent::MessageDelta(crate::types::MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some("end_turn".to_string()),
                    stop_sequence: None,
                },
                usage: Usage::default(),
            }))
        );
    }

    #[test]
    fn debug_json_suffix_keeps_end_of_payload_for_parse_diagnostics() {
        let payload = r#"{"type":"message_delta","usage":{"cache_creation":null}}"#;

        let suffix = json_debug_suffix(payload, 28);
        assert!(suffix.contains("cache_creation"));
        assert!(suffix.contains("null"));
    }

    #[test]
    fn sse_frame_payload_logging_skips_successful_parse_frames() {
        assert!(!should_log_sse_frame_payload("parse"));
        assert!(should_log_sse_frame_payload("done"));
    }
}
