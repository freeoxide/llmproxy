//! Buffered SSE parser for upstream provider streams: reassembles frames split
//! across TCP chunks and handles comments, event/id fields, and multi-line data.

use std::str;

use crate::error::ProviderError;

/// A parsed SSE frame, emitted only after a blank line boundary or when
/// [`SseFramer::finish`] is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// Optional `event:` field value.
    pub event: Option<String>,
    /// Optional `id:` field value.
    pub id: Option<String>,
    /// Collected `data:` field value(s), joined with `\n` for multi-line data.
    pub data: String,
}

/// Default ceiling on the framer's byte buffer and on a single frame's
/// accumulated `data:` payload; exceeding either yields [`ProviderError::SseFraming`].
pub const DEFAULT_MAX_BUFFER_BYTES: usize = 1024 * 1024;

/// Buffered SSE frame parser: feed raw bytes via [`SseFramer::push_chunk`] as
/// they arrive; call [`SseFramer::finish`] at stream end to flush a partial frame.
#[derive(Debug)]
pub struct SseFramer {
    buffer: Vec<u8>,
    max_buffer_bytes: usize,
    current_event: Option<String>,
    current_id: Option<String>,
    current_data_lines: Vec<String>,
    current_data_bytes: usize,
    bom_checked: bool,
}

impl SseFramer {
    /// Create a new empty framer with the default buffer cap
    /// ([`DEFAULT_MAX_BUFFER_BYTES`]).
    pub fn new() -> Self {
        Self::with_max_buffer_bytes(DEFAULT_MAX_BUFFER_BYTES)
    }

    /// Create a new empty framer with a custom cap bounding both a single
    /// unterminated line and the accumulated `data:` payload of a single frame.
    pub fn with_max_buffer_bytes(max_buffer_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_buffer_bytes,
            current_event: None,
            current_id: None,
            current_data_lines: Vec::new(),
            current_data_bytes: 0,
            bom_checked: false,
        }
    }

    /// Feed a raw byte chunk (possibly partial lines/UTF-8) and return frames
    /// completed at a blank-line boundary; errors on invalid UTF-8 or cap overrun.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ProviderError> {
        self.buffer.extend_from_slice(chunk);

        if !self.bom_checked {
            const BOM: &[u8; 3] = &[0xEF, 0xBB, 0xBF];
            if self.buffer.starts_with(BOM) {
                self.buffer.drain(..BOM.len());
            }
            let still_prefix = (1..BOM.len())
                .any(|n| self.buffer.len() == n && BOM.starts_with(&self.buffer[..n]));
            if !still_prefix {
                self.bom_checked = true;
            }
        }

        if self.buffer.len() > self.max_buffer_bytes {
            return Err(ProviderError::SseFraming(format!(
                "SSE input exceeded the {}-byte line buffer without a newline \
                 (max_buffer_bytes); aborting to avoid unbounded growth",
                self.max_buffer_bytes
            )));
        }
        self.drain_buffer()
    }

    /// Flush remaining buffered data as a final frame; call when the upstream
    /// stream ends, emitting any trailing partial frame.
    pub fn finish(&mut self) -> Result<Vec<SseFrame>, ProviderError> {
        let mut frames = Vec::new();

        if !self.buffer.is_empty() {
            self.flush_complete_lines(&mut frames)?;

            if !self.buffer.is_empty() {
                let text = str::from_utf8(&self.buffer).map_err(ProviderError::from)?;
                let line = text.trim_end_matches('\r').to_owned();
                self.buffer.clear();
                self.process_line(&line)?;
            }
        }

        if let Some(frame) = self.take_current_frame() {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn flush_complete_lines(&mut self, frames: &mut Vec<SseFrame>) -> Result<(), ProviderError> {
        let Some(last_nl) = self.buffer.iter().rposition(|&b| b == b'\n') else {
            return Ok(());
        };
        let complete = self.buffer[..last_nl + 1].to_vec();
        self.buffer.drain(..last_nl + 1);
        let text = str::from_utf8(&complete).map_err(ProviderError::from)?;
        for line in text.lines() {
            let trimmed = line.trim_end_matches('\r');
            if !trimmed.is_empty() {
                self.process_line(trimmed)?;
            } else if let Some(frame) = self.take_current_frame() {
                frames.push(frame);
            }
        }
        Ok(())
    }

    fn drain_buffer(&mut self) -> Result<Vec<SseFrame>, ProviderError> {
        let mut frames = Vec::new();

        while let Some(nl_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let trimmed_end = match self.buffer[..nl_pos].iter().rposition(|&b| b != b'\r') {
                Some(pos) => pos + 1,
                None => 0,
            };

            let line = str::from_utf8(&self.buffer[..trimmed_end])
                .map_err(ProviderError::from)?
                .to_owned();

            self.buffer.drain(..nl_pos + 1);

            if line.is_empty() {
                if let Some(frame) = self.take_current_frame() {
                    frames.push(frame);
                }
            } else {
                self.process_line(&line)?;
            }
        }

        Ok(frames)
    }

    fn process_line(&mut self, line: &str) -> Result<(), ProviderError> {
        if line.starts_with(':') {
            return Ok(());
        }

        if let Some(colon_pos) = line.find(':') {
            let field = &line[..colon_pos];
            let value = &line[colon_pos + 1..];
            let value = value.strip_prefix(' ').unwrap_or(value);

            match field {
                "event" => {
                    self.current_event = Some(value.to_owned());
                }
                "id" => {
                    self.current_id = Some(value.to_owned());
                }
                "data" => {
                    self.push_data(value)?;
                }
                _ => {}
            }
        } else {
            match line {
                "event" => {
                    self.current_event = Some(String::new());
                }
                "id" => {
                    self.current_id = Some(String::new());
                }
                "data" => {
                    self.push_data("")?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn push_data(&mut self, value: &str) -> Result<(), ProviderError> {
        let next = self.current_data_bytes.saturating_add(value.len());
        if next > self.max_buffer_bytes {
            return Err(ProviderError::SseFraming(format!(
                "SSE frame exceeded the {}-byte data payload cap without a \
                 blank-line boundary (max_buffer_bytes); aborting to avoid \
                 unbounded growth",
                self.max_buffer_bytes
            )));
        }
        self.current_data_bytes = next;
        self.current_data_lines.push(value.to_owned());
        Ok(())
    }

    fn take_current_frame(&mut self) -> Option<SseFrame> {
        let data_lines = std::mem::take(&mut self.current_data_lines);
        self.current_data_bytes = 0;
        if data_lines.is_empty() && self.current_event.is_none() && self.current_id.is_none() {
            return None;
        }

        let data = data_lines.join("\n");
        Some(SseFrame {
            event: self.current_event.take(),
            id: self.current_id.take(),
            data,
        })
    }
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_framer_handles_partial_frames_split_across_chunks() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: hel").unwrap();
        assert!(frames.is_empty(), "No complete frame yet");

        let frames = framer.push_chunk(b"lo\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_handles_multi_line_data_fields() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"data: line1\ndata: line2\ndata: line3\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn sse_framer_handles_crlf() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: hello\r\n\r\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_ignores_comments() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b": this is a comment\ndata: hello\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_preserves_event_and_id() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"event: message_start\nid: 42\ndata: {\"type\":\"start\"}\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("message_start"));
        assert_eq!(frames[0].id.as_deref(), Some("42"));
        assert_eq!(frames[0].data, "{\"type\":\"start\"}");
    }

    #[test]
    fn sse_framer_emits_trailing_frame_from_finish() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: partial").unwrap();
        assert!(frames.is_empty());

        let frames = framer.finish().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "partial");
    }

    #[test]
    fn sse_framer_respects_blank_line_frame_boundaries() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"data: first\n\ndata: second\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "first");
        assert_eq!(frames[1].data, "second");
    }

    #[test]
    fn sse_framer_preserves_done() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"data: {\"content\":\"hi\"}\n\ndata: [DONE]\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "{\"content\":\"hi\"}");
        assert_eq!(frames[1].data, "[DONE]");
    }

    #[test]
    fn sse_framer_rejects_invalid_utf8() {
        let mut framer = SseFramer::new();

        let result = framer.push_chunk(&[0xFF, 0xFE]);
        assert!(
            result.is_ok(),
            "push_chunk should buffer raw bytes without error"
        );
        let result = framer.finish();
        assert!(
            result.is_err(),
            "Expected error for invalid UTF-8 on finish"
        );
        match result.unwrap_err() {
            ProviderError::Utf8(_) => {}
            other => panic!("Expected ProviderError::Utf8, got: {:?}", other),
        }
    }

    #[test]
    fn sse_framer_rejects_invalid_utf8_at_newline() {
        let mut framer = SseFramer::new();

        let result = framer.push_chunk(&[0xFF, 0xFE, b'\n']);
        assert!(
            result.is_err(),
            "Expected error for invalid UTF-8 at newline"
        );
        match result.unwrap_err() {
            ProviderError::Utf8(_) => {}
            other => panic!("Expected ProviderError::Utf8, got: {:?}", other),
        }
    }

    #[test]
    fn sse_framer_multiple_frames_across_many_chunks() {
        let mut framer = SseFramer::new();
        let mut all_frames = Vec::new();

        all_frames.extend(framer.push_chunk(b"data: a").unwrap());
        all_frames.extend(framer.push_chunk(b"\n\nda").unwrap());
        all_frames.extend(framer.push_chunk(b"ta: b\n\nda").unwrap());
        all_frames.extend(framer.push_chunk(b"ta: c\n\n").unwrap());

        assert_eq!(all_frames.len(), 3);
        assert_eq!(all_frames[0].data, "a");
        assert_eq!(all_frames[1].data, "b");
        assert_eq!(all_frames[2].data, "c");
    }

    #[test]
    fn sse_framer_event_without_data() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"event: ping\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("ping"));
        assert_eq!(frames[0].data, "");
    }

    #[test]
    fn sse_framer_mixed_crlf_and_lf() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"data: first\r\n\r\ndata: second\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "first");
        assert_eq!(frames[1].data, "second");
    }

    #[test]
    fn sse_framer_id_and_event_reset_per_frame() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"event: msg1\nid: 1\ndata: hello\n\ndata: world\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("msg1"));
        assert_eq!(frames[0].id.as_deref(), Some("1"));
        assert_eq!(frames[0].data, "hello");
        assert!(frames[1].event.is_none());
        assert!(frames[1].id.is_none());
        assert_eq!(frames[1].data, "world");
    }

    #[test]
    fn sse_framer_empty_input_returns_no_frames() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"").unwrap();
        assert!(frames.is_empty(), "Empty chunk should produce no frames");

        let frames = framer.finish().unwrap();
        assert!(
            frames.is_empty(),
            "Finish on empty framer should produce no frames"
        );
    }

    #[test]
    fn sse_framer_only_comments_yields_no_frames() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b": comment one\n: comment two\n\n")
            .unwrap();
        assert!(
            frames.is_empty(),
            "Comments-only stream should produce no frames"
        );

        let frames = framer.finish().unwrap();
        assert!(
            frames.is_empty(),
            "Finish after comments-only stream should produce no frames"
        );
    }

    #[test]
    fn sse_framer_only_blank_lines_yields_no_frames() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"\n\n\n\n").unwrap();
        assert!(
            frames.is_empty(),
            "Blank lines without data fields should produce no frames"
        );

        let frames = framer.finish().unwrap();
        assert!(
            frames.is_empty(),
            "Finish after blank lines should produce no frames"
        );
    }

    #[test]
    fn sse_framer_data_field_empty_value() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data:\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "");
    }

    #[test]
    fn sse_framer_finish_after_complete_stream_is_empty() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: hello\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");

        let frames = framer.finish().unwrap();
        assert!(
            frames.is_empty(),
            "Finish after fully drained stream should produce no frames"
        );
    }

    #[test]
    fn sse_framer_ignores_unknown_fields() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"retry: 5000\ndata: hello\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_preserves_url_with_multiple_colons() {
        let mut framer = SseFramer::new();

        let frames = framer
            .push_chunk(b"data: http://example.com:8080/api\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "http://example.com:8080/api");
    }

    #[test]
    fn sse_framer_finish_trims_trailing_cr() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: hello\r").unwrap();
        assert!(frames.is_empty());

        let frames = framer.finish().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_multibyte_utf8_split_across_chunks() {
        let full_char = "あ";
        let full_bytes = full_char.as_bytes();
        assert_eq!(full_bytes.len(), 3);

        let mut framer = SseFramer::new();

        let mut chunk1 = b"data: ".to_vec();
        chunk1.extend_from_slice(&full_bytes[..2]);
        let frames = framer.push_chunk(&chunk1).unwrap();
        assert!(frames.is_empty(), "No complete frame yet");

        let mut chunk2 = full_bytes[2..].to_vec();
        chunk2.extend_from_slice(b"\n\n");
        let frames = framer.push_chunk(&chunk2).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].data, "あ",
            "Multi-byte char must be preserved across chunk split"
        );
    }

    #[test]
    fn sse_framer_data_newline_then_finish() {
        let mut framer = SseFramer::new();
        let frames = framer.push_chunk(b"data: hello\n").unwrap();
        assert!(frames.is_empty(), "No blank line yet, no frame");

        let frames = framer.finish().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_reusable_after_finish() {
        let mut framer = SseFramer::new();

        let frames = framer.push_chunk(b"data: first\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "first");

        let frames = framer.finish().unwrap();
        assert!(frames.is_empty(), "Nothing left after complete stream");

        let frames = framer.push_chunk(b"data: second\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "second");
    }

    #[test]
    fn sse_framer_rejects_unterminated_line_exceeding_cap() {
        let mut framer = SseFramer::with_max_buffer_bytes(64);
        let big = b"x".repeat(65);
        let err = framer.push_chunk(&big).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line buffer") && msg.contains("64"),
            "expected line-buffer cap error mentioning 64, got: {msg}"
        );
    }

    #[test]
    fn sse_framer_rejects_oversized_data_frame_exceeding_cap() {
        let mut framer = SseFramer::with_max_buffer_bytes(64);
        let line = b"data: aaaaaaaa\n"; // 8 payload bytes, no blank line
        let mut last_ok = true;
        for _ in 0..16 {
            let res = framer.push_chunk(line);
            match res {
                Ok(_) => last_ok = true,
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("data payload cap") && msg.contains("64"),
                        "expected frame cap error mentioning 64, got: {msg}"
                    );
                    last_ok = false;
                    break;
                }
            }
        }
        assert!(
            !last_ok,
            "expected the frame cap to trip before 16 lines, but it never did"
        );
    }

    #[test]
    fn sse_framer_resets_data_byte_counter_after_frame_boundary() {
        let mut framer = SseFramer::with_max_buffer_bytes(32);
        for i in 0..50 {
            let chunk = format!("data: item{i}\n\n");
            let frames = framer.push_chunk(chunk.as_bytes()).unwrap();
            assert_eq!(frames.len(), 1, "frame {i} should emit");
            assert_eq!(frames[0].data, format!("item{i}"));
        }
    }

    #[test]
    fn sse_framer_default_cap_is_one_mebibyte() {
        assert_eq!(DEFAULT_MAX_BUFFER_BYTES, 1024 * 1024);
    }

    #[test]
    fn sse_framer_strips_single_leading_bom() {
        let mut framer = SseFramer::new();
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"data: hello\n\n");
        let frames = framer.push_chunk(&input).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_strips_bom_split_across_chunks() {
        let mut framer = SseFramer::new();
        let frames = framer.push_chunk(&[0xEF, 0xBB]).unwrap();
        assert!(frames.is_empty(), "no complete frame yet");
        let mut second = vec![0xBF];
        second.extend_from_slice(b"data: hello\n\n");
        let frames = framer.push_chunk(&second).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_framer_strips_only_one_bom() {
        let mut framer = SseFramer::new();
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"data: ");
        input.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        input.extend_from_slice(b"hello\n\n");
        let frames = framer.push_chunk(&input).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "\u{FEFF}hello");
    }

    #[test]
    fn sse_framer_no_bom_preserves_leading_content() {
        let mut framer = SseFramer::new();
        let frames = framer.push_chunk(b"data: hello\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn sse_source_no_forbidden_imports() {
        let source = include_str!("sse.rs");
        let prod = source
            .split_once("#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(source);
        assert!(
            !prod.contains("llm_proxy_protocol"),
            "sse.rs must not import llm_proxy_protocol"
        );
        assert!(
            !prod.contains("EndpointType"),
            "sse.rs must not mention EndpointType"
        );
        assert!(
            !prod.contains("OpenCodeClient"),
            "sse.rs must not mention OpenCodeClient"
        );
        assert!(
            !prod.contains("classify_endpoint"),
            "sse.rs must not mention classify_endpoint"
        );
        assert!(
            !prod.contains("is_anthropic_model"),
            "sse.rs must not mention provider model classification"
        );
        assert!(
            !prod.contains("is_gemini_model"),
            "sse.rs must not mention provider model classification"
        );
    }
}
