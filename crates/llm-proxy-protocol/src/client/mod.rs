//! Client protocol adapters: each translates between one client wire format
//! and the normalised core protocol types, never another adapter's format.

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai_chat;
pub mod responses;

/// Dialects that correlate tool results by name (Gemini `functionResponse`,
/// Ollama `tool` messages) synthesize ids for core `ToolUse` blocks and then
/// match each result to the earliest unmatched call with that name, in
/// conversation order. A result naming no call keeps its name as the id.
pub(crate) fn correlate_tool_results_by_name(messages: &mut [crate::core::CoreMessage]) {
    use crate::core::CoreContent;
    use std::collections::{HashMap, VecDeque};

    let mut open: HashMap<String, VecDeque<String>> = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let CoreContent::ToolUse { id, name, .. } = block {
                open.entry(name.clone()).or_default().push_back(id.clone());
            }
        }
        for block in &mut message.content {
            if let CoreContent::ToolResult { tool_use_id, .. } = block
                && let Some(queue) = open.get_mut(tool_use_id.as_str())
                && let Some(matched) = queue.pop_front()
            {
                *tool_use_id = matched;
            }
        }
    }
}

/// Errors returned by client protocol adapters. Route handlers map
/// `InvalidRequest`/`Decode` to 400 and `Encode` to 500 (502 for
/// unrepresentable upstream data); no status is carried, so context wins.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The client request could not be decoded into core types.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// An error occurred while encoding core types to the client protocol.
    #[error("encode error: {0}")]
    Encode(String),
    /// An error occurred while decoding the client protocol into core types.
    #[error("decode error: {0}")]
    Decode(String),
    /// A content block was dropped because the client protocol does not support
    /// it, but the omission is safe (e.g. Document/Audio/Video in Anthropic
    /// responses). The block was already logged with a `tracing::warn!`.
    #[error("skippable encode: {0}")]
    EncodeSkippable(String),
}
