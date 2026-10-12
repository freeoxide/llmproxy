//! OpenAI Chat Completions provider adapter.
//!
//! Translates between core types and the OpenAI Chat Completions wire format:
//!
//! ```text
//! CoreRequest  -> ChatCompletionRequest
//! ChatCompletionResponse -> CoreResponse
//! ChatCompletionChunk stream -> CoreEvent stream
//! ```

use std::collections::HashMap;

use llm_proxy_protocol::core::{
    ContentKind, CoreContent, CoreEvent, CoreRequest, CoreResponse, CoreRole, CoreToolChoice,
    ModelRef, StopReason,
};
use llm_proxy_protocol::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, FunctionCall, FunctionDef,
    StreamOptions, ToolCall, ToolDef,
};

use super::{
    ProviderAdapterTarget, ProviderStreamDecoder, build_proxy_request, build_usage_from_openai,
    expand_url_template, map_openai_finish_reason, response_model_ref,
};
use crate::error::ProviderError;
use crate::sse::SseFrame;

// ---------------------------------------------------------------------------
// Adapter struct
// ---------------------------------------------------------------------------

/// Adapter for the OpenAI Chat Completions API.
#[derive(Debug, Clone, Default)]
pub struct OpenAiChatAdapter;

// ---------------------------------------------------------------------------
// Stream decoder
// ---------------------------------------------------------------------------

/// Stateful stream decoder for OpenAI Chat Completions SSE frames.
///
/// ## CoreEvent variants emitted
///
/// - `MessageStart` -- on the first chunk received
/// - `ContentStart` -- on first text or reasoning delta
/// - `TextDelta` -- on `delta.content`
/// - `ThinkingDelta` -- on `delta.reasoning_content` (or `delta.reasoning`)
/// - `ToolCallStart` -- on new tool calls in `delta.tool_calls`
/// - `ToolCallDelta` -- on tool call argument deltas
/// - `ToolCallStop` -- on stream end for unclosed tool blocks
/// - `UsageDelta` -- on usage-only chunks or final chunks with usage
/// - `MessageStop` -- on `finish_reason` or stream end
///
/// Intentionally never emitted: `Ping` (OpenAI Chat has no ping mechanism),
/// `Error` (OpenAI errors are handled at the transport level, not in stream
/// decoding).
#[derive(Debug)]
pub struct OpenAiChatStreamDecoder {
    model_ref: ModelRef,
    started: bool,
    content_index: usize,
    content_started: bool,
    reasoning_started: bool,
    /// Maps OpenAI tool-call index to our content block index.
    tool_blocks: HashMap<usize, usize>,
    stop_sent: bool,
}

impl ProviderStreamDecoder for OpenAiChatStreamDecoder {
    fn decode_frame(&mut self, frame: &SseFrame) -> Result<Vec<CoreEvent>, ProviderError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(vec![]);
        }

        let chunk: ChatCompletionChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => {
                let truncated = super::truncate_str_safe(data, 200);
                tracing::warn!(data = truncated, "malformed OpenAI chunk, skipping");
                return Ok(vec![]);
            }
        };

        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            events.push(CoreEvent::MessageStart {
                id: Some(chunk.id.clone()),
                model: self.model_ref.clone(),
            });
        }

        // Usage-only chunk (no choices).
        if chunk.choices.is_empty() {
            if let Some(ref usage) = chunk.usage {
                events.push(CoreEvent::UsageDelta {
                    usage: build_usage_from_openai(
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.prompt_cache_hit_tokens,
                        usage.prompt_cache_miss_tokens,
                    ),
                });
            }
            return Ok(events);
        }

        let choice = &chunk.choices[0];

        // Handle reasoning content (both `reasoning_content` and `reasoning`).
        // The serde alias on ChatMessage ensures both field names deserialize
        // into `reasoning_content`.
        //
        // Empty-string reasoning deltas are intentionally skipped: OpenAI sends
        // them as keep-alives with no semantic meaning, and usage-only /
        // finish-reason-only chunks are handled in separate code paths below.
        if let Some(reasoning) = choice
            .delta
            .as_ref()
            .and_then(|d| d.reasoning_content.as_ref())
        {
            if !reasoning.is_empty() {
                self.close_text_if_open();
                if !self.reasoning_started {
                    self.reasoning_started = true;
                    events.push(CoreEvent::ContentStart {
                        index: self.content_index,
                        kind: ContentKind::Thinking,
                    });
                }
                events.push(CoreEvent::ThinkingDelta {
                    index: self.content_index,
                    text: reasoning.clone(),
                });
            }
        }

        // Handle text content deltas.
        //
        // Empty-string content deltas are intentionally skipped: OpenAI sends
        // them as keep-alives with no semantic meaning.  The finish_reason and
        // usage are handled in separate code paths, so this truthy check does not
        // drop meaningful events.
        if let Some(ref delta) = choice.delta {
            if !delta.content_is_empty() {
                self.close_reasoning_if_open();
                if !self.content_started {
                    self.content_started = true;
                    events.push(CoreEvent::ContentStart {
                        index: self.content_index,
                        kind: ContentKind::Text,
                    });
                }
                events.push(CoreEvent::TextDelta {
                    index: self.content_index,
                    text: delta.content_text(),
                });
            }
        }

        // Handle tool call deltas.
        if let Some(ref delta) = choice.delta
            && !delta.tool_calls.is_empty()
        {
            self.close_content_if_open();
            self.push_tool_call_deltas(&delta.tool_calls, &mut events);
        }

        // Handle finish reason.
        if let Some(ref reason) = choice.finish_reason {
            if !reason.is_empty() && !self.stop_sent {
                self.close_content_if_open();
                self.close_tool_blocks(&mut events);

                let stop_reason = map_openai_finish_reason(reason);

                if let Some(ref usage) = chunk.usage {
                    events.push(CoreEvent::UsageDelta {
                        usage: build_usage_from_openai(
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            usage.prompt_cache_hit_tokens,
                            usage.prompt_cache_miss_tokens,
                        ),
                    });
                }

                events.push(CoreEvent::MessageStop {
                    stop_reason,
                    stop_sequence: None,
                });
                self.stop_sent = true;
            }
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<CoreEvent>, ProviderError> {
        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            events.push(CoreEvent::MessageStart {
                id: None,
                model: self.model_ref.clone(),
            });
        }

        self.close_content_if_open();
        self.close_tool_blocks(&mut events);

        if !self.stop_sent {
            let stop_reason = if !self.tool_blocks.is_empty() {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            events.push(CoreEvent::MessageStop {
                stop_reason,
                stop_sequence: None,
            });
            self.stop_sent = true;
        }

        Ok(events)
    }
}

impl OpenAiChatStreamDecoder {
    /// Close any open text content block.  Reserved for future ContentStop emission.
    fn close_text_if_open(&mut self) {
        if self.content_started {
            self.content_started = false;
            self.content_index += 1;
        }
    }

    /// Close any open reasoning content block.  Reserved for future ContentStop emission.
    fn close_reasoning_if_open(&mut self) {
        if self.reasoning_started {
            self.reasoning_started = false;
            self.content_index += 1;
        }
    }

    /// Close any open text or reasoning content blocks.
    fn close_content_if_open(&mut self) {
        if self.content_started {
            self.content_started = false;
            self.content_index += 1;
        }
        if self.reasoning_started {
            self.reasoning_started = false;
            self.content_index += 1;
        }
    }

    /// Translates a chunk's `tool_calls` array into core tool-call events.
    ///
    /// Opens a block the first time an upstream index is seen and routes
    /// argument fragments to the block that index owns, so parallel calls stay
    /// separated.
    fn push_tool_call_deltas(&mut self, tool_calls: &[ToolCall], events: &mut Vec<CoreEvent>) {
        for tc in tool_calls {
            // The wire `index` is `Option<i32>`; negative values are
            // semantically meaningless but a malformed/malicious chunk could
            // carry one.  An unchecked `as usize` would wrap (e.g. `-1` ->
            // `usize::MAX`), producing phantom tool blocks with huge indices.
            // Validate before casting: negatives fall back to 0 and are logged.
            let oi = tc
                .index
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or_else(|| {
                    if tc.index.is_some_and(|i| i < 0) {
                        tracing::warn!(
                            raw_index = tc.index,
                            "OpenAI: negative tool_call index, clamping to 0"
                        );
                    }
                    0
                });

            // New tool call?
            if !self.tool_blocks.contains_key(&oi) {
                let func_name = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.name.as_deref())
                    .unwrap_or("");
                if func_name.is_empty() {
                    tracing::warn!(
                        index = oi,
                        "OpenAI: tool_call without function name, skipping tool call"
                    );
                    continue;
                }

                let tool_id = tc
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4()));

                let block_idx = self.content_index;
                self.tool_blocks.insert(oi, block_idx);

                events.push(CoreEvent::ToolCallStart {
                    index: block_idx,
                    id: tool_id,
                    name: func_name.to_owned(),
                });

                self.content_index += 1;
            }

            // Argument delta.
            if let Some(ref func) = tc.function
                && let Some(ref args) = func.arguments
                && !args.is_empty()
                && let Some(&block_idx) = self.tool_blocks.get(&oi)
            {
                events.push(CoreEvent::ToolCallDelta {
                    index: block_idx,
                    args_delta: args.clone(),
                });
            }
        }
    }

    fn close_tool_blocks(&self, events: &mut Vec<CoreEvent>) {
        let mut indices: Vec<_> = self.tool_blocks.values().copied().collect();
        indices.sort();
        for idx in indices {
            events.push(CoreEvent::ToolCallStop { index: idx });
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAiChatAdapter impl
// ---------------------------------------------------------------------------

/// Folds one assistant content block into the outgoing tool calls / reasoning.
///
/// Blocks the Chat Completions schema cannot carry are dropped with a warning.
fn encode_assistant_content(
    content: &CoreContent,
    tool_calls: &mut Vec<ToolCall>,
    reasoning_content: &mut Option<String>,
) {
    match content {
        CoreContent::Thinking { text, .. } => {
            if !text.is_empty() {
                match reasoning_content {
                    Some(existing) => existing.push_str(text),
                    None => *reasoning_content = Some(text.clone()),
                }
            }
        }
        CoreContent::ToolUse { id, name, input } => {
            let arguments = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_owned());
            tool_calls.push(ToolCall {
                index: None,
                id: (!id.is_empty()).then(|| id.clone()),
                r#type: Some("function".to_owned()),
                function: Some(FunctionCall {
                    name: (!name.is_empty()).then(|| name.clone()),
                    arguments: Some(arguments),
                }),
            });
        }
        other => {
            tracing::warn!(
                ?other,
                "OpenAI Chat: dropping unsupported content block in assistant message encoding"
            );
        }
    }
}

/// Appends one `role: "tool"` message per tool result in `content`.
///
/// Chat Completions carries tool results as standalone messages rather than as
/// content blocks on the user turn.
fn push_tool_result_messages(content: &[CoreContent], messages: &mut Vec<ChatMessage>) {
    for c in content {
        let CoreContent::ToolResult {
            tool_use_id,
            content: result_content,
            ..
        } = c
        else {
            continue;
        };
        messages.push(ChatMessage {
            role: "tool".to_owned(),
            content: serde_json::Value::String(collect_text(result_content)),
            reasoning_content: None,
            tool_calls: Vec::new(),
            name: None,
            tool_call_id: Some(tool_use_id.clone()),
            cache_control: None,
            refusal: None,
            is_error: None,
        });
    }
}

impl OpenAiChatAdapter {
    /// Encode a core request into an OpenAI Chat Completions request.
    #[expect(
        clippy::too_many_lines,
        reason = "field-by-field wire encode of the OpenAI Chat Completions request \
                 shape, kept in one place so the mapping can be audited against \
                 the API schema without jumping between helpers"
    )]
    pub fn encode_request(
        &self,
        core: &CoreRequest,
        target: &ProviderAdapterTarget,
    ) -> Result<super::ProxyRequest, ProviderError> {
        let mut messages = Vec::new();

        for sys_content in &core.system {
            match sys_content {
                CoreContent::Text { text, cache } => {
                    if cache.is_some() {
                        tracing::warn!(
                            "OpenAI Chat: cache_control is not supported, dropping cache marker on system message"
                        );
                    }
                    if !text.is_empty() {
                        messages.push(ChatMessage {
                            role: "system".to_owned(),
                            content: serde_json::Value::String(text.clone()),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                            name: None,
                            tool_call_id: None,
                            cache_control: None,
                            refusal: None,
                            is_error: None,
                        });
                    }
                }
                other => {
                    tracing::warn!(
                        ?other,
                        "OpenAI Chat: dropping non-Text system content block during encode"
                    );
                }
            }
        }

        for msg in &core.messages {
            match msg.role {
                CoreRole::User => {
                    let text = collect_text(&msg.content);
                    if !text.is_empty() {
                        messages.push(ChatMessage {
                            role: "user".to_owned(),
                            content: serde_json::Value::String(text),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                            name: None,
                            tool_call_id: None,
                            cache_control: None,
                            refusal: None,
                            is_error: None,
                        });
                    }

                    // Tool results become separate tool messages.
                    push_tool_result_messages(&msg.content, &mut messages);
                }
                CoreRole::Assistant => {
                    let text = collect_text(&msg.content);
                    let mut tool_calls = Vec::new();
                    let mut reasoning_content: Option<String> = None;

                    for content in &msg.content {
                        encode_assistant_content(content, &mut tool_calls, &mut reasoning_content);
                    }

                    // When an assistant turn carries only tool calls (no text),
                    // emit `content: null` rather than `content: ""`.  The
                    // OpenAI Chat Completions API tolerates both, but some
                    // strict OpenAI-compatible upstreams (e.g. Ollama) treat an
                    // empty string differently from null/absent and drop out of
                    // tool-calling mode.  `null` is the conventionally-correct
                    // representation and is accepted by both OpenAI and Ollama.
                    let content = if text.is_empty() && !tool_calls.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(text)
                    };

                    messages.push(ChatMessage {
                        role: "assistant".to_owned(),
                        content,
                        reasoning_content,
                        tool_calls,
                        name: None,
                        tool_call_id: None,
                        cache_control: None,
                        refusal: None,
                        is_error: None,
                    });
                }
                CoreRole::System => {
                    let text = collect_text(&msg.content);
                    if !text.is_empty() {
                        messages.push(ChatMessage {
                            role: "system".to_owned(),
                            content: serde_json::Value::String(text),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                            name: None,
                            tool_call_id: None,
                            cache_control: None,
                            refusal: None,
                            is_error: None,
                        });
                    }
                }
                CoreRole::Tool => {
                    let text = collect_text(&msg.content);
                    let tool_use_id = msg
                        .content
                        .iter()
                        .find_map(|c| match c {
                            CoreContent::ToolResult { tool_use_id, .. } => {
                                Some(tool_use_id.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    if tool_use_id.is_empty() {
                        tracing::warn!(
                            "OpenAI Chat: Tool message has empty tool_use_id; downstream protocols may reject this"
                        );
                    }
                    messages.push(ChatMessage {
                        role: "tool".to_owned(),
                        content: serde_json::Value::String(text),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        name: None,
                        tool_call_id: Some(tool_use_id),
                        cache_control: None,
                        refusal: None,
                        is_error: None,
                    });
                }
                _ => {
                    // Handle future CoreRole variants as user messages.
                    let text = collect_text(&msg.content);
                    if !text.is_empty() {
                        messages.push(ChatMessage {
                            role: "user".to_owned(),
                            content: serde_json::Value::String(text),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                            name: None,
                            tool_call_id: None,
                            cache_control: None,
                            refusal: None,
                            is_error: None,
                        });
                    }
                }
            }
        }

        let tools: Vec<ToolDef> = core
            .tools
            .iter()
            .map(|t| {
                let schema = if t.input_schema.is_null() {
                    serde_json::json!({"type": "object", "properties": {}})
                } else {
                    t.input_schema.clone()
                };
                ToolDef {
                    r#type: "function".to_owned(),
                    function: FunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: Some(schema),
                    },
                }
            })
            .collect();

        // Tool choice -- omit (None) for unknown variants instead of sending null.
        let tool_choice = core.tool_choice.as_ref().and_then(|tc| match tc {
            CoreToolChoice::Auto => Some(serde_json::json!({"type": "auto"})),
            CoreToolChoice::Any => Some(serde_json::json!({"type": "required"})),
            CoreToolChoice::None => Some(serde_json::json!({"type": "none"})),
            CoreToolChoice::Tool { name } => Some(serde_json::json!({
                "type": "function",
                "function": {"name": name}
            })),
            CoreToolChoice::Raw(v) => Some(v.clone()),
            _ => {
                tracing::warn!(?tc, "OpenAI Chat: unknown tool_choice variant, omitting");
                None
            }
        });

        let mut req = ChatCompletionRequest {
            model: target.upstream_model.clone(),
            messages,
            stream: if core.stream { Some(true) } else { None },
            temperature: core.sampling.temperature,
            top_p: core.sampling.top_p,
            max_tokens: core.sampling.max_tokens,
            reasoning_effort: core.sampling.reasoning_effort.clone(),
            thinking: core.sampling.thinking.clone(),
            tools,
            tool_choice,
            stop: core.sampling.stop.as_ref().map(|v| {
                if v.len() == 1 {
                    serde_json::Value::String(v[0].clone())
                } else {
                    // Manual serialization avoids an infallible expect.
                    // Vec<String> always serializes to a JSON array of strings.
                    serde_json::Value::Array(
                        v.iter()
                            .map(|s| serde_json::Value::String(s.clone()))
                            .collect(),
                    )
                }
            }),
            stream_options: None,
            user: core.metadata.user_id.clone(),
            extra: serde_json::Map::new(),
        };

        // Propagate the client's include_usage preference to the upstream request.
        // If the client didn't request usage, we don't ask upstream for it either.
        if core.stream {
            let include_usage = core
                .provider_hints
                .raw
                .get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            req.stream_options = Some(StreamOptions {
                include_usage: Some(include_usage),
            });
        }

        let body = serde_json::to_vec(&req)?;
        let url = expand_url_template(&target.endpoint, target)?;

        Ok(build_proxy_request(body, target, core.stream, url))
    }

    /// Decode an OpenAI Chat Completions response body.
    pub fn decode_response(
        &self,
        bytes: &[u8],
        target: &ProviderAdapterTarget,
    ) -> Result<CoreResponse, ProviderError> {
        let resp: llm_proxy_protocol::openai::ChatCompletionResponse =
            serde_json::from_slice(bytes)?;

        if resp.choices.is_empty() {
            return Err(ProviderError::EmptyResponse(
                "no choices in response".to_owned(),
            ));
        }

        let choice = &resp.choices[0];
        let msg = choice
            .message
            .as_ref()
            .ok_or_else(|| ProviderError::SseFraming("choice has no message".to_owned()))?;

        let mut content = Vec::new();

        // Reasoning -> Thinking (typically comes first from the provider).
        if let Some(ref reasoning) = msg.reasoning_content {
            if !reasoning.is_empty() {
                content.push(CoreContent::Thinking {
                    text: reasoning.clone(),
                    signature: None,
                });
            }
        }

        // Text content (may appear before or after tool calls).
        if !msg.content_is_empty() {
            content.push(CoreContent::Text {
                text: msg.content_text(),
                cache: None,
            });
        }

        // Refusal.
        if let Some(ref refusal) = msg.refusal {
            if !refusal.is_empty() {
                content.push(CoreContent::Refusal {
                    text: refusal.clone(),
                });
            }
        }

        // Tool calls -> ToolUse.
        for tc in &msg.tool_calls {
            let raw_args = tc.function.as_ref().and_then(|f| f.arguments.as_ref());
            let input = raw_args
                .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
                .unwrap_or_else(|| {
                    if raw_args.is_some() {
                        tracing::debug!(
                            "OpenAI: tool_call.arguments was not valid JSON, using empty object"
                        );
                    }
                    serde_json::Value::Object(serde_json::Map::new())
                });

            content.push(CoreContent::ToolUse {
                id: tc.id.clone().unwrap_or_else(|| {
                    tracing::warn!("OpenAI: tool_call missing id in response decode");
                    "<unknown_tool_id>".to_owned()
                }),
                name: tc
                    .function
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_else(|| {
                        tracing::warn!("OpenAI: tool_call missing name in response decode");
                        "<unknown_tool>".to_owned()
                    }),
                input,
            });
        }

        // Guarantee at least one content block.
        if content.is_empty() {
            content.push(CoreContent::Text {
                text: String::new(),
                cache: None,
            });
        }

        let stop_reason = choice
            .finish_reason
            .as_deref()
            .map(map_openai_finish_reason)
            // Provider omitted `finish_reason`; record that it was absent
            // rather than unmapped so the diagnostic is unambiguous.
            .unwrap_or(StopReason::Unknown("absent".to_owned()));

        let usage = build_usage_from_openai(
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
            resp.usage.prompt_cache_hit_tokens,
            resp.usage.prompt_cache_miss_tokens,
        );

        Ok(CoreResponse {
            id: Some(resp.id),
            model: response_model_ref(target),
            content,
            stop_reason,
            stop_sequence: None,
            usage,
            provider_meta: serde_json::Map::new(),
            cost: None,
        })
    }

    /// Create a new stream decoder.
    pub fn new_stream_decoder(&self, target: &ProviderAdapterTarget) -> OpenAiChatStreamDecoder {
        OpenAiChatStreamDecoder {
            model_ref: response_model_ref(target),
            started: false,
            content_index: 0,
            content_started: false,
            reasoning_started: false,
            tool_blocks: HashMap::new(),
            stop_sent: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all text from a list of content blocks.
///
/// Non-text blocks are silently omitted, but a warning is logged since this
/// means data is being dropped.  Per the plan's lossy translation rules, every
/// dropped feature must be either rejected, warned, or preserved opaquely.
fn collect_text(content: &[CoreContent]) -> String {
    let mut text = String::new();
    for c in content {
        match c {
            CoreContent::Text { text: t, cache } => {
                if cache.is_some() {
                    tracing::warn!(
                        "OpenAI Chat: cache_control is not supported, dropping cache marker"
                    );
                }
                text.push_str(t);
            }
            other => {
                tracing::warn!(
                    ?other,
                    "OpenAI Chat: dropping non-text content block during collect_text"
                );
            }
        }
    }
    text
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use llm_proxy_core::AuthStyle;
    use llm_proxy_protocol::core::{CoreMessage, CoreTool, SamplingOptions};
    use llm_proxy_protocol::openai::{Choice, UsageInfo};

    fn make_target() -> ProviderAdapterTarget {
        ProviderAdapterTarget {
            provider_name: "test-openai".into(),
            adapter_name: "openai-chat".into(),
            protocol: super::super::ProviderProtocol::OpenAiChatCompletions,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            auth_style: AuthStyle::Bearer,
            api_key: secrecy::SecretString::from("test-key"),
            requested_model: "gpt-4o".into(),
            upstream_model: "gpt-4o-2024-08-06".into(),
            headers: std::sync::Arc::new(std::collections::HashMap::new()),
        }
    }

    fn make_core_request(messages: Vec<CoreMessage>) -> CoreRequest {
        CoreRequest {
            model: ModelRef {
                requested: "gpt-4o".into(),
                upstream: None,
            },
            system: vec![],
            messages,
            tools: vec![],
            tool_choice: None,
            sampling: SamplingOptions::default(),
            stream: false,
            metadata: Default::default(),
            provider_hints: Default::default(),
        }
    }

    // -- encode_request tests ------------------------------------------------

    #[test]
    fn encode_text_request() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "Hello".into(),
                cache: None,
            }],
        }]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.model, "gpt-4o-2024-08-06");
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(
            body.messages[0].content,
            serde_json::Value::String("Hello".into())
        );
    }

    #[test]
    fn encode_system_prompt() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.system = vec![CoreContent::Text {
            text: "You are helpful".into(),
            cache: None,
        }];
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(
            body.messages[0].content,
            serde_json::Value::String("You are helpful".into())
        );
    }

    #[test]
    fn encode_tool_declarations() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.tools = vec![CoreTool {
            name: "get_weather".into(),
            description: Some("Get weather".into()),
            input_schema: serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }];
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].function.name, "get_weather");
    }

    #[test]
    fn encode_tool_choice() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.tool_choice = Some(CoreToolChoice::Auto);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.tool_choice, Some(serde_json::json!({"type": "auto"})));
    }

    #[test]
    fn encode_tool_result() {
        let core = make_core_request(vec![
            CoreMessage {
                role: CoreRole::User,
                content: vec![CoreContent::Text {
                    text: "use tool".into(),
                    cache: None,
                }],
            },
            CoreMessage {
                role: CoreRole::Assistant,
                content: vec![CoreContent::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "SF"}),
                }],
            },
            CoreMessage {
                role: CoreRole::User,
                content: vec![CoreContent::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![CoreContent::Text {
                        text: "72F sunny".into(),
                        cache: None,
                    }],
                    is_error: false,
                }],
            },
        ]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.messages.len(), 3);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(body.messages[1].role, "assistant");
        assert_eq!(body.messages[1].tool_calls.len(), 1);
        assert_eq!(body.messages[2].role, "tool");
        assert_eq!(body.messages[2].tool_call_id, Some("call_1".to_owned()));
        assert_eq!(
            body.messages[2].content,
            serde_json::Value::String("72F sunny".into())
        );
    }

    #[test]
    fn encode_stream_sets_stream_options() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.stream = true;
        // Simulate the client requesting usage via provider_hints.
        core.provider_hints.raw.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.stream, Some(true));
        assert!(body.stream_options.is_some());
        assert_eq!(body.stream_options.unwrap().include_usage, Some(true));
        assert!(proxy_req.stream);
    }

    #[test]
    fn encode_stream_without_usage_hint_omits_usage() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.stream = true;
        // No provider_hints set -- include_usage should default to false.
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.stream, Some(true));
        assert!(body.stream_options.is_some());
        assert_eq!(body.stream_options.unwrap().include_usage, Some(false));
    }

    #[test]
    fn encode_uses_upstream_model() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.model, "gpt-4o-2024-08-06");
    }

    #[test]
    fn encode_sampling_options() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling = SamplingOptions {
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(1024),
            stop: Some(vec!["END".into()]),
            ..Default::default()
        };
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.temperature, Some(0.7));
        assert_eq!(body.top_p, Some(0.9));
        assert_eq!(body.max_tokens, Some(1024));
        assert_eq!(body.stop, Some(serde_json::json!("END")));
    }

    // -- decode_response tests -----------------------------------------------

    #[test]
    fn decode_text_response() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 12345,
            model: "gpt-4o-2024-08-06".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("hello world".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: None,
                    is_error: None,
                }),
                finish_reason: Some("stop".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                prompt_cache_hit_tokens: Some(10),
                prompt_cache_miss_tokens: Some(20),
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.id, Some("chatcmpl-test".to_owned()));
        assert_eq!(core_resp.model.requested, "gpt-4o");
        assert_eq!(core_resp.stop_reason, StopReason::EndTurn);
        assert_eq!(core_resp.usage.input_tokens, 70);
        assert_eq!(core_resp.usage.output_tokens, 50);
        assert_eq!(core_resp.content.len(), 1);
        assert_eq!(
            core_resp.content[0],
            CoreContent::Text {
                text: "hello world".into(),
                cache: None,
            }
        );
    }

    #[test]
    fn decode_tool_call_response() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String(String::new()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        index: None,
                        id: Some("call_123".into()),
                        r#type: Some("function".into()),
                        function: Some(FunctionCall {
                            name: Some("get_weather".into()),
                            arguments: Some(r#"{"city":"SF"}"#.into()),
                        }),
                    }],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: None,
                    is_error: None,
                }),
                finish_reason: Some("tool_calls".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 200,
                completion_tokens: 80,
                total_tokens: 280,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.stop_reason, StopReason::ToolUse);
        let tool = &core_resp.content[0];
        match tool {
            CoreContent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            _ => panic!("expected ToolUse, got {:?}", tool),
        }
    }

    #[test]
    fn decode_response_preserves_requested_model() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o-2024-08-06".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("hi".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: None,
                    is_error: None,
                }),
                finish_reason: Some("stop".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.model.requested, "gpt-4o");
    }

    // -- Streaming tests -----------------------------------------------------

    #[test]
    fn stream_text_decoding() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }
        all_events.extend(decoder.finish().unwrap());

        assert_has_event(&all_events, "MessageStart");
        assert_has_event(&all_events, "ContentStart");
        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::TextDelta { text, .. } if text == "Hello"
        )));
        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::TextDelta { text, .. } if text == " world"
        )));
        assert_has_event(&all_events, "MessageStop");
    }

    #[test]
    fn stream_tool_call_decoding() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"calling"},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"SF\"}"}}]},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::ToolCallStart { name, .. } if name == "get_weather"
        )));
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallDelta { .. }))
        );
        assert_has_event(&all_events, "ToolCallStop");
        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn stream_negative_tool_index_clamped() {
        // A malformed/malicious chunk carrying a negative `index` must not wrap
        // to usize::MAX and create a phantom tool block.  It should be clamped
        // to 0 and still produce a valid ToolCallStart at index 0.
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":-1,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
        );
        let events = decoder.decode_frame(&frame).unwrap();

        // Exactly one ToolCallStart, and its content_index is 0 (not a wrapped
        // huge value).
        let starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                CoreEvent::ToolCallStart { index, name, .. } => Some((*index, name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "expected exactly one ToolCallStart");
        assert_eq!(starts[0].0, 0, "negative index must clamp to 0");
        assert_eq!(starts[0].1, "get_weather");
    }

    #[test]
    fn encode_assistant_only_tool_calls_emits_null_content() {
        // An assistant turn with only ToolUse blocks (no text) must serialize
        // `content: null`, not `content: ""`, for compatibility with strict
        // OpenAI-compatible upstreams.
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::Assistant,
            content: vec![CoreContent::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "SF"}),
            }],
        }]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.messages.len(), 1);
        assert_eq!(
            body.messages[0].content,
            serde_json::Value::Null,
            "assistant-only-tool-calls must encode content as null"
        );
        assert_eq!(body.messages[0].tool_calls.len(), 1);
    }

    #[test]
    fn encode_assistant_with_text_keeps_string_content() {
        // An assistant turn with real text must keep `content` as the string
        // (regression guard for the null-content fix).
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::Assistant,
            content: vec![
                CoreContent::Text {
                    text: "thinking...".into(),
                    cache: None,
                },
                CoreContent::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "SF"}),
                },
            ],
        }]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(
            body.messages[0].content,
            serde_json::Value::String("thinking...".into())
        );
    }

    #[test]
    fn stream_done_marker_ignored() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("[DONE]");
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn stream_malformed_frame_skipped() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("{not valid json}");
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn stream_finish_emits_lifecycle() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let events = decoder.finish().unwrap();
        assert_has_event(&events, "MessageStart");
        assert_has_event(&events, "MessageStop");
    }

    // -- Missing tests -------------------------------------------------------

    #[test]
    fn decode_thinking_response() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "deepseek-chat".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("answer".into()),
                    reasoning_content: Some("Let me think...".into()),
                    tool_calls: vec![],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: None,
                    is_error: None,
                }),
                finish_reason: Some("stop".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        let thinking = core_resp
            .content
            .iter()
            .find(|c| matches!(c, CoreContent::Thinking { .. }));
        assert!(thinking.is_some(), "expected Thinking content");
    }

    #[test]
    fn decode_reasoning_alias_field() {
        // Verify that the `reasoning` field alias works via serde.
        let json = r#"{"role":"assistant","content":"hi","reasoning":"thinking tokens"}"#;
        let msg: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.reasoning_content, Some("thinking tokens".to_owned()));
    }

    #[test]
    fn decode_refusal_response() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String(String::new()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: Some("I cannot help with that.".into()),
                    is_error: None,
                }),
                finish_reason: Some("stop".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        let refusal = core_resp
            .content
            .iter()
            .find(|c| matches!(c, CoreContent::Refusal { .. }));
        assert!(refusal.is_some(), "expected Refusal content");
    }

    #[test]
    fn decode_empty_choices_returns_error() {
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: vec![],
            usage: UsageInfo {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let result = adapter.decode_response(&bytes, &target);
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::EmptyResponse(_) => {}
            other => panic!("expected EmptyResponse, got {:?}", other),
        }
    }

    #[test]
    fn stream_usage_only_chunk() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![make_frame(
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::UsageDelta { .. })),
            "usage-only chunk must emit UsageDelta, got: {:?}",
            all_events
        );
    }

    #[test]
    fn stream_reasoning_alias() {
        // Verify that streaming chunks with `reasoning` (not `reasoning_content`) work.
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"reasoning":"thinking..."},"finish_reason":null}]}"#,
        );
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ThinkingDelta { .. })),
            "reasoning alias must produce ThinkingDelta"
        );
    }

    // -- Additional missing tests ----------------------------------------------

    #[test]
    fn encode_reasoning_effort_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.reasoning_effort = Some("high".into());
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.reasoning_effort, Some("high".to_owned()));
    }

    #[test]
    fn encode_stop_sequences() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.stop = Some(vec!["END".into(), "STOP".into()]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.stop, Some(serde_json::json!(["END", "STOP"])));
    }

    #[test]
    fn encode_stop_single_sequence_scalar() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.stop = Some(vec!["END".into()]);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.stop, Some(serde_json::json!("END")));
    }

    #[test]
    fn encode_thinking_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.thinking =
            Some(serde_json::json!({"type": "enabled", "budget_tokens": 5000}));
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert!(body.thinking.is_some());
        assert_eq!(body.thinking.unwrap()["budget_tokens"], 5000);
    }

    #[test]
    fn encode_tool_choice_none_omitted() {
        // Verify that unknown tool_choice variants produce None (omitted).
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        // Raw null is a valid variant, but we can test with Auto to ensure it's Some.
        core.tool_choice = Some(CoreToolChoice::Auto);
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert!(body.tool_choice.is_some());
    }

    #[test]
    fn encode_input_schema_null_coerced() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.tools = vec![CoreTool {
            name: "my_tool".into(),
            description: None,
            input_schema: serde_json::Value::Null,
        }];
        let adapter = OpenAiChatAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ChatCompletionRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let schema = &body.tools[0].function.parameters;
        assert_eq!(schema.as_ref().unwrap()["type"], "object");
    }

    #[test]
    fn decode_preserves_content_order_thinking_before_text() {
        // Verify that Thinking appears before Text (provider order).
        let target = make_target();
        let resp = llm_proxy_protocol::openai::ChatCompletionResponse {
            id: "test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "deepseek-chat".into(),
            choices: vec![Choice {
                index: 0,
                message: Some(ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("answer".into()),
                    reasoning_content: Some("thoughts".into()),
                    tool_calls: vec![],
                    name: None,
                    tool_call_id: None,
                    cache_control: None,
                    refusal: None,
                    is_error: None,
                }),
                finish_reason: Some("stop".into()),
                delta: None,
            }],
            usage: UsageInfo {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            extra: serde_json::Map::new(),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = OpenAiChatAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        // Thinking must come before Text.
        let thinking_idx = core_resp
            .content
            .iter()
            .position(|c| matches!(c, CoreContent::Thinking { .. }))
            .unwrap();
        let text_idx = core_resp
            .content
            .iter()
            .position(|c| matches!(c, CoreContent::Text { .. }))
            .unwrap();
        assert!(
            thinking_idx < text_idx,
            "Thinking must precede Text in content order"
        );
    }

    #[test]
    fn stream_full_lifecycle_ordering() {
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
            ),
            make_frame(
                r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        let start_idx = all_events
            .iter()
            .position(|e| matches!(e, CoreEvent::MessageStart { .. }))
            .unwrap();
        let content_start_idx = all_events
            .iter()
            .position(|e| matches!(e, CoreEvent::ContentStart { .. }))
            .unwrap();
        let text_delta_idx = all_events
            .iter()
            .position(|e| matches!(e, CoreEvent::TextDelta { .. }))
            .unwrap();
        let stop_idx = all_events
            .iter()
            .rposition(|e| matches!(e, CoreEvent::MessageStop { .. }))
            .unwrap();

        assert!(
            start_idx < content_start_idx,
            "MessageStart must precede ContentStart"
        );
        assert!(
            content_start_idx < text_delta_idx,
            "ContentStart must precede TextDelta"
        );
        assert!(
            text_delta_idx < stop_idx,
            "TextDelta must precede MessageStop"
        );
    }

    #[test]
    fn stream_finish_reason_only_chunk() {
        // Test a chunk that only has finish_reason with no prior content.
        let target = make_target();
        let adapter = OpenAiChatAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![make_frame(
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        )];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. }))
        );
        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                ..
            }
        )));
    }

    // -- Source guard ---------------------------------------------------------

    #[test]
    fn adapter_source_no_forbidden_imports() {
        let source = include_str!("openai_chat.rs");
        let prod = source
            .split_once("#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(source);
        assert!(
            !prod.contains("llm_proxy_protocol::client"),
            "adapter must not import client protocol types"
        );
        assert!(
            !prod.contains("llm_proxy_server"),
            "adapter must not import server crate"
        );
    }

    // -- Helpers --------------------------------------------------------------

    fn make_frame(data: &str) -> crate::sse::SseFrame {
        crate::sse::SseFrame {
            event: None,
            id: None,
            data: data.to_owned(),
        }
    }

    fn assert_has_event(events: &[CoreEvent], name: &str) {
        let found = match name {
            "MessageStart" => events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. })),
            "ContentStart" => events
                .iter()
                .any(|e| matches!(e, CoreEvent::ContentStart { .. })),
            "MessageStop" => events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStop { .. })),
            "ToolCallStop" => events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallStop { .. })),
            _ => false,
        };
        assert!(found, "expected {} event, got: {:?}", name, events);
    }
}
