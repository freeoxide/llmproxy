//! OpenAI Responses API provider adapter.
//!
//! Translates between core types and the Responses API wire format:
//!
//! ```text
//! CoreRequest  -> ResponsesRequest
//! ResponsesResponse -> CoreResponse
//! ResponsesChunk stream -> CoreEvent stream
//! ```

use std::collections::HashMap;

use llm_proxy_protocol::core::{
    ContentKind, CoreContent, CoreEvent, CoreRequest, CoreResponse, CoreRole, CoreToolChoice,
    ModelRef, StopReason, Usage, UsageProvenance,
};
use llm_proxy_protocol::zen::{
    ResponsesChunk, ResponsesInput, ResponsesOutput, ResponsesReasoning, ResponsesRequest,
    ResponsesResponse, ResponsesTool, ResponsesUsage,
};

use super::{
    ProviderAdapterTarget, ProviderStreamDecoder, build_proxy_request, expand_url_template,
    response_model_ref,
};
use crate::error::ProviderError;
use crate::sse::SseFrame;

// ---------------------------------------------------------------------------
// Adapter struct
// ---------------------------------------------------------------------------

/// Adapter for the OpenAI Responses API.
#[derive(Debug, Clone, Default)]
pub struct ResponsesAdapter;

// ---------------------------------------------------------------------------
// Stream decoder
// ---------------------------------------------------------------------------

/// Stateful stream decoder for Responses API SSE frames.
///
/// ## CoreEvent variants emitted
///
/// - `MessageStart` -- on `response.created` or `response.in_progress`
/// - `ContentStart` -- on `response.output_text.delta` (text) or
///   `response.output_item.added` with `function_call` (tool use)
/// - `TextDelta` -- on `response.output_text.delta`
/// - `ToolCallStart` -- on `response.output_item.added` with `function_call`
///   (one per upstream `output_index`, supporting parallel calls)
/// - `ToolCallDelta` -- on `response.function_call_arguments.delta`
///   (routed to the open block matching the chunk's `output_index`)
/// - `ToolCallStop` -- on `response.function_call_arguments.done`; any blocks
///   still open are closed on `response.completed`/`response.done`/`finish()`
/// - `UsageDelta` -- on `response.completed` or `response.done` with usage
/// - `MessageStop` -- on `response.completed` or `response.done`
/// - `Error` -- on `response.failed`
///
/// Intentionally never emitted: `Ping`, `ThinkingDelta` (the Responses API
/// does not produce heartbeat or thinking-stream events).
#[derive(Debug)]
pub struct ResponsesStreamDecoder {
    model_ref: ModelRef,
    started: bool,
    content_index: usize,
    content_started: bool,
    /// Open tool-call blocks keyed by the upstream `output_index`, mapping to
    /// the core `content_index` assigned when the block started. The Responses
    /// API interleaves parallel function-call argument deltas and
    /// disambiguates them via `output_index`, so the decoder must track one
    /// block per concurrent call rather than a single open call.
    tool_blocks: HashMap<usize, usize>,
    /// Whether any tool call was seen during this stream.
    saw_tool_call: bool,
    stop_sent: bool,
}

impl ProviderStreamDecoder for ResponsesStreamDecoder {
    #[expect(
        clippy::too_many_lines,
        reason = "one match arm per Responses API stream event: the wire shape \
                 is a single translation table, and splitting the arms into \
                 helpers would separate each event's field mapping from the \
                 frame layout it produces"
    )]
    fn decode_frame(&mut self, frame: &SseFrame) -> Result<Vec<CoreEvent>, ProviderError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(vec![]);
        }

        let chunk: ResponsesChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => {
                let truncated = super::truncate_str_safe(data, 200);
                tracing::warn!(data = truncated, "malformed Responses chunk, skipping");
                return Ok(vec![]);
            }
        };

        let mut events = Vec::new();

        match chunk.r#type.as_str() {
            "response.created" | "response.in_progress" => {
                if !self.started {
                    self.started = true;
                    events.push(CoreEvent::MessageStart {
                        id: chunk.id.clone(),
                        model: self.model_ref.clone(),
                    });
                }
            }
            "response.output_item.added" => {
                let oi = chunk.output_index.unwrap_or(0);
                for output in chunk.output.iter().flatten() {
                    if output.r#type == "message" {
                        self.start_text_block_if_needed(output, &mut events);
                    } else if output.r#type == "function_call" {
                        self.start_function_call(oi, output, &mut events);
                    }
                }
            }
            "response.output_text.delta" => {
                if let Some(ref delta) = chunk.delta
                    && !delta.is_empty()
                {
                    if !self.content_started {
                        self.content_started = true;
                        events.push(CoreEvent::ContentStart {
                            index: self.content_index,
                            kind: ContentKind::Text,
                        });
                    }
                    events.push(CoreEvent::TextDelta {
                        index: self.content_index,
                        text: delta.clone(),
                    });
                }
            }
            "response.output_text.done" => {
                if self.content_started {
                    self.content_started = false;
                    self.content_index += 1;
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(ref delta) = chunk.delta
                    && !delta.is_empty()
                {
                    let oi = chunk.output_index.unwrap_or(0);
                    // If no block is open for this output_index (e.g. a missed
                    // response.output_item.added event), synthesize a start
                    // keyed by it before routing the delta.
                    let block_idx = self.ensure_tool_block(
                        oi,
                        "no prior response.output_item.added event",
                        &mut events,
                    );
                    events.push(CoreEvent::ToolCallDelta {
                        index: block_idx,
                        args_delta: delta.clone(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                let oi = chunk.output_index.unwrap_or(0);
                // If no block is open for this output_index (e.g. a missed
                // response.output_item.added event), emit a synthetic start so
                // the stop has a matching ToolCallStart, then close it.
                self.ensure_tool_block(
                    oi,
                    "no prior response.output_item.added event before arguments.done",
                    &mut events,
                );
                if let Some(block_idx) = self.tool_blocks.remove(&oi) {
                    events.push(CoreEvent::ToolCallStop { index: block_idx });
                }
            }
            "response.completed" => {
                self.close_content_if_open();
                self.close_tool_blocks(&mut events);

                // Extract usage.
                if let Some(ref usage) = chunk.usage {
                    events.push(CoreEvent::UsageDelta {
                        usage: build_responses_usage(usage),
                    });
                }

                // Extract stop reason from output if present, falling back to
                // saw_tool_call for streams where function_call output items
                // may not be present on the response.completed event.
                let stop_reason = self.infer_stop_reason(chunk.output.as_deref());

                if !self.stop_sent {
                    self.stop_sent = true;
                    events.push(CoreEvent::MessageStop {
                        stop_reason,
                        stop_sequence: None,
                    });
                }
            }
            "response.done" => {
                self.close_content_if_open();
                self.close_tool_blocks(&mut events);

                if let Some(ref usage) = chunk.usage {
                    events.push(CoreEvent::UsageDelta {
                        usage: build_responses_usage(usage),
                    });
                }

                if !self.stop_sent {
                    self.stop_sent = true;
                    let stop_reason = self.infer_stop_reason(chunk.output.as_deref());
                    events.push(CoreEvent::MessageStop {
                        stop_reason,
                        stop_sequence: None,
                    });
                }
            }
            "response.failed" => {
                let message = chunk
                    .error
                    .as_ref()
                    .and_then(|e| e.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("response failed")
                    .to_owned();
                // Map error code to more specific CoreStreamErrorKind when available.
                let kind = chunk
                    .error
                    .as_ref()
                    .and_then(|e| e.get("code").and_then(|c| c.as_str()))
                    .map(|code| match code {
                        "rate_limit_exceeded" | "429" => {
                            llm_proxy_protocol::core::CoreStreamErrorKind::RateLimit
                        }
                        "invalid_request_error" | "400" => {
                            llm_proxy_protocol::core::CoreStreamErrorKind::InvalidRequest
                        }
                        "authentication_error" | "401" => {
                            llm_proxy_protocol::core::CoreStreamErrorKind::Authentication
                        }
                        "server_error" | "502" | "503" => {
                            llm_proxy_protocol::core::CoreStreamErrorKind::Upstream
                        }
                        _ => llm_proxy_protocol::core::CoreStreamErrorKind::Upstream,
                    })
                    .unwrap_or(llm_proxy_protocol::core::CoreStreamErrorKind::Upstream);
                events.push(CoreEvent::Error {
                    error: llm_proxy_protocol::core::CoreStreamError::new(kind, message),
                });
            }
            _ => {
                // Unknown event type; skip.
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

        // Flush any unclosed tool calls (e.g. a stream that emitted
        // ToolCallStart/arguments.delta but ended without arguments.done or
        // response.completed), so downstream consumers receive a balanced
        // ToolCallStart/ToolCallStop pair. Mirrors the Gemini/OpenAI-chat
        // decoders' unclosed-tool-block handling in finish().
        self.close_tool_blocks(&mut events);

        if !self.stop_sent {
            self.stop_sent = true;
            let stop_reason = if self.saw_tool_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            events.push(CoreEvent::MessageStop {
                stop_reason,
                stop_sequence: None,
            });
        }

        Ok(events)
    }
}

impl ResponsesStreamDecoder {
    fn close_content_if_open(&mut self) {
        if self.content_started {
            self.content_started = false;
            self.content_index += 1;
        }
    }

    /// Emit `ToolCallStop` for every still-open tool block (sorted by
    /// `content_index` for a deterministic, ascending close order) and clear
    /// the map. Mirrors the Gemini/OpenAI-chat decoders' `close_tool_blocks`
    /// so every `ToolCallStart` is guaranteed a matching `ToolCallStop`.
    /// Opens a text content block when a `message` output first carries
    /// `output_text` and none is currently open.
    fn start_text_block_if_needed(
        &mut self,
        output: &ResponsesOutput,
        events: &mut Vec<CoreEvent>,
    ) {
        if self.content_started {
            return;
        }
        let has_text = output
            .content
            .iter()
            .flatten()
            .any(|content| content.r#type == "output_text");
        if has_text {
            self.content_started = true;
            events.push(CoreEvent::ContentStart {
                index: self.content_index,
                kind: ContentKind::Text,
            });
        }
    }

    /// Opens a tool-call block for `oi` unless one is already open.
    ///
    /// Parallel calls are disambiguated by the upstream `output_index`, so one
    /// block is tracked per concurrent call.
    fn start_function_call(
        &mut self,
        oi: usize,
        output: &ResponsesOutput,
        events: &mut Vec<CoreEvent>,
    ) {
        self.saw_tool_call = true;
        if self.tool_blocks.contains_key(&oi) {
            return;
        }
        self.close_content_if_open();
        let call_id = output.call_id.clone().unwrap_or_else(|| {
            tracing::warn!("Responses: function_call output missing call_id");
            "<unknown_tool_id>".to_owned()
        });
        let name = output.name.clone().unwrap_or_else(|| {
            tracing::warn!("Responses: function_call output missing name");
            "<unknown_tool>".to_owned()
        });
        let block_idx = self.content_index;
        self.tool_blocks.insert(oi, block_idx);
        self.content_index += 1;
        events.push(CoreEvent::ToolCallStart {
            index: block_idx,
            id: call_id,
            name,
        });
    }

    /// Returns the content index of the tool block for `oi`, synthesizing a
    /// `ToolCallStart` (logged with `warn_message`) when the upstream never sent
    /// a `response.output_item.added` for it.
    fn ensure_tool_block(
        &mut self,
        oi: usize,
        warn_message: &'static str,
        events: &mut Vec<CoreEvent>,
    ) -> usize {
        if let Some(&block_idx) = self.tool_blocks.get(&oi) {
            return block_idx;
        }
        self.close_content_if_open();
        let block_idx = self.content_index;
        let synthetic_id = format!("__responses_missing_{block_idx}__");
        tracing::warn!(
            synthetic_id = synthetic_id,
            output_index = oi,
            reason = warn_message,
            "Responses: emitting synthetic ToolCallStart"
        );
        self.tool_blocks.insert(oi, block_idx);
        self.content_index += 1;
        events.push(CoreEvent::ToolCallStart {
            index: block_idx,
            id: synthetic_id,
            name: "<missing_function_name>".to_owned(),
        });
        block_idx
    }

    fn close_tool_blocks(&mut self, events: &mut Vec<CoreEvent>) {
        let mut indices: Vec<_> = self.tool_blocks.values().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            events.push(CoreEvent::ToolCallStop { index: idx });
        }
        self.tool_blocks.clear();
    }

    /// Infer the stop reason from output items and whether a tool call was seen.
    ///
    /// Checks for `function_call` output items in the chunk. If none are found,
    /// falls back to the `saw_tool_call` flag that was set during streaming.
    fn infer_stop_reason(&self, outputs: Option<&[ResponsesOutput]>) -> StopReason {
        if let Some(outs) = outputs {
            if outs.iter().any(|o| o.r#type == "function_call") {
                return StopReason::ToolUse;
            }
        }
        if self.saw_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }
    }
}

// ---------------------------------------------------------------------------
// ResponsesAdapter impl
// ---------------------------------------------------------------------------

/// Folds one core content block into the Responses input item list.
///
/// Text accumulates in `text_parts` so consecutive text blocks collapse into a
/// single input item; a tool block flushes that buffer first so a message never
/// emits duplicate role entries.
fn encode_content_item<'a>(
    c: &'a CoreContent,
    role: &str,
    text_parts: &mut Vec<&'a str>,
    input: &mut Vec<ResponsesInput>,
) {
    match c {
        CoreContent::Text { text, cache } => {
            if cache.is_some() {
                tracing::warn!("Responses: cache_control is not supported, dropping cache marker");
            }
            if !text.is_empty() {
                text_parts.push(text.as_str());
            }
        }
        CoreContent::ToolUse {
            id,
            name,
            input: tool_input,
        } => {
            // Flush pending text as its own input item first.
            if !text_parts.is_empty() {
                input.push(ResponsesInput {
                    role: role.to_owned(),
                    content: Some(serde_json::Value::String(text_parts.join(""))),
                });
                text_parts.clear();
            }

            // The Responses API represents prior tool calls as input items with
            // type "function_call".
            let arguments = serde_json::to_string(tool_input).unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "Responses: failed to serialize tool input, falling back to empty object"
                );
                "{}".to_owned()
            });
            input.push(ResponsesInput {
                role: role.to_owned(),
                content: Some(serde_json::json!({
                    "type": "function_call",
                    "call_id": id.clone(),
                    "name": name.clone(),
                    "arguments": arguments,
                })),
            });
        }
        CoreContent::ToolResult {
            tool_use_id,
            content: result_content,
            ..
        } => {
            let result_text: String = result_content
                .iter()
                .filter_map(|rc| match rc {
                    CoreContent::Text { text, .. } => Some(text.as_str()),
                    other => {
                        tracing::warn!(
                            ?other,
                            tool_use_id,
                            "Responses: dropping non-text content block in ToolResult encoding"
                        );
                        None
                    }
                })
                .collect();
            input.push(ResponsesInput {
                role: role.to_owned(),
                content: Some(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": result_text,
                })),
            });
        }
        _ => {
            tracing::warn!(
                role,
                ?c,
                "dropping unsupported content block during Responses encode"
            );
        }
    }
}

/// Collects the text of every content block on a `message` output.
///
/// Unknown block types still contribute their `text` field, if any, rather than
/// being dropped silently.
fn push_message_text(output: &ResponsesOutput, content: &mut Vec<CoreContent>) {
    for block in output.content.iter().flatten() {
        if block.r#type != "output_text" {
            tracing::warn!(
                block_type = block.r#type,
                "Responses: unknown content block type in message output, extracting text if present"
            );
        }
        if let Some(ref text) = block.text {
            content.push(CoreContent::Text {
                text: text.clone(),
                cache: None,
            });
        }
    }
}

impl ResponsesAdapter {
    /// Encode a core request into a Responses API request.
    #[expect(
        clippy::too_many_lines,
        reason = "field-by-field wire encode of the Responses API request shape, \
                 kept in one place so the mapping can be audited against the \
                 API schema without jumping between helpers"
    )]
    pub fn encode_request(
        &self,
        core: &CoreRequest,
        target: &ProviderAdapterTarget,
    ) -> Result<super::ProxyRequest, ProviderError> {
        let mut input = Vec::new();

        // System prompt as first developer message.
        if !core.system.is_empty() {
            // Warn about non-text system content blocks (consistent with other adapters).
            for c in &core.system {
                if !matches!(c, CoreContent::Text { .. }) {
                    tracing::warn!(
                        ?c,
                        "Responses: dropping non-Text system content block during encode"
                    );
                }
            }
            let system_text: String = core
                .system
                .iter()
                .filter_map(|c| match c {
                    CoreContent::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .fold(String::new(), |mut acc, s| {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(s);
                    acc
                });
            if !system_text.is_empty() {
                input.push(ResponsesInput {
                    role: "developer".to_owned(),
                    content: Some(serde_json::Value::String(system_text)),
                });
            }
        }

        // Conversation messages.
        for msg in &core.messages {
            let role = match msg.role {
                CoreRole::User => "user",
                CoreRole::Assistant => "assistant",
                CoreRole::System => "developer",
                CoreRole::Tool => "user",
                // Future CoreRole variants are mapped to "user" as a safe default.
                // Update this match when new variants are added to CoreRole.
                _ => "user",
            };

            // Build content from all supported types, not just text.
            let mut text_parts = Vec::new();

            for c in &msg.content {
                encode_content_item(c, role, &mut text_parts, &mut input);
            }

            // Flush any remaining text content as a separate same-role input
            // item. Trailing text after a tool block (e.g. an assistant message
            // shaped [ToolUse, Text] or [ToolResult, Text]) must be preserved,
            // and the Responses API permits multiple input items with the same
            // role, so flush unconditionally rather than gating on the absence
            // of function_call/function_call_output.
            if !text_parts.is_empty() {
                let text: String = text_parts.join("");
                input.push(ResponsesInput {
                    role: role.to_owned(),
                    content: Some(serde_json::Value::String(text)),
                });
            }
        }

        // Tools.
        let tools: Vec<ResponsesTool> = core
            .tools
            .iter()
            .map(|t| ResponsesTool {
                r#type: "function".to_owned(),
                name: Some(t.name.clone()),
                description: t.description.clone(),
                parameters: Some(if !t.input_schema.is_object() {
                    serde_json::json!({"type": "object", "properties": {}})
                } else {
                    t.input_schema.clone()
                }),
            })
            .collect();

        // Reasoning effort.
        let reasoning = core
            .sampling
            .reasoning_effort
            .as_ref()
            .map(|effort| ResponsesReasoning {
                effort: Some(effort.clone()),
            });

        let mut req = ResponsesRequest {
            model: target.upstream_model.clone(),
            input,
            stream: if core.stream { Some(true) } else { None },
            tools,
            reasoning,
            tool_choice: None,
        };

        // Warn about sampling fields that the Responses API does not support.
        if core.sampling.temperature.is_some() {
            tracing::warn!(
                "Responses: temperature specified but not forwarded; \
                 the Responses API does not support this parameter"
            );
        }
        if core.sampling.top_p.is_some() {
            tracing::warn!(
                "Responses: top_p specified but not forwarded; \
                 the Responses API does not support this parameter"
            );
        }
        if core.sampling.max_tokens.is_some() {
            tracing::warn!(
                "Responses: max_tokens specified but not forwarded; \
                 the Responses API does not support this parameter"
            );
        }
        if core.sampling.stop.as_ref().is_some_and(|s| !s.is_empty()) {
            tracing::warn!(
                "Responses: stop sequences specified but not forwarded; \
                 the Responses API does not support this parameter"
            );
        }
        if core.metadata.user_id.is_some() {
            tracing::warn!(
                "Responses: user_id specified but not forwarded; \
                 the Responses API does not support this parameter"
            );
        }

        // Forward tool_choice if present.  Omit unknown variants instead of
        // sending null (per the plan's lossy translation rules).
        if let Some(ref tc) = core.tool_choice {
            req.tool_choice = match tc {
                CoreToolChoice::Auto => Some(serde_json::json!({"type": "auto"})),
                CoreToolChoice::Any => Some(serde_json::json!({"type": "required"})),
                CoreToolChoice::None => Some(serde_json::json!({"type": "none"})),
                CoreToolChoice::Tool { name } => Some(serde_json::json!({
                    "type": "function",
                    "name": name
                })),
                CoreToolChoice::Raw(v) => Some(v.clone()),
                _ => {
                    tracing::warn!(?tc, "Responses: unknown tool_choice variant, omitting");
                    None
                }
            };
        }

        let body = serde_json::to_vec(&req)?;
        let url = expand_url_template(&target.endpoint, target)?;

        Ok(build_proxy_request(body, target, core.stream, url))
    }

    /// Decode a Responses API response body.
    pub fn decode_response(
        &self,
        bytes: &[u8],
        target: &ProviderAdapterTarget,
    ) -> Result<CoreResponse, ProviderError> {
        let resp: ResponsesResponse = serde_json::from_slice(bytes)?;

        let mut content = Vec::new();

        for output in &resp.output {
            match output.r#type.as_str() {
                "message" => {
                    push_message_text(output, &mut content);
                }
                "function_call" => {
                    let input = output
                        // Lazily build the empty object only on the `None`/parse-fail path
                        // so the common tool-calling happy path avoids a heap allocation (MEDIUM-4).
                        .arguments
                        .as_ref()
                        .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

                    content.push(CoreContent::ToolUse {
                        id: output.call_id.clone().unwrap_or_default(),
                        name: output.name.clone().unwrap_or_default(),
                        input,
                    });
                }
                _ => {
                    tracing::warn!(
                        output_type = output.r#type,
                        "Responses: skipping unknown output type in decode_response"
                    );
                }
            }
        }

        // Guarantee at least one content block.
        if content.is_empty() {
            content.push(CoreContent::Text {
                text: String::new(),
                cache: None,
            });
        }

        let has_tool_use = content
            .iter()
            .any(|c| matches!(c, CoreContent::ToolUse { .. }));
        let stop_reason = match resp.status.as_deref() {
            Some("failed") | Some("expired") => StopReason::Error,
            Some("incomplete") => StopReason::MaxTokens,
            _ if has_tool_use => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };

        let usage = build_responses_usage(&resp.usage);

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
    pub fn new_stream_decoder(&self, target: &ProviderAdapterTarget) -> ResponsesStreamDecoder {
        ResponsesStreamDecoder {
            model_ref: response_model_ref(target),
            started: false,
            content_index: 0,
            content_started: false,
            tool_blocks: HashMap::new(),
            saw_tool_call: false,
            stop_sent: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build usage from Responses API usage.
fn build_responses_usage(usage: &ResponsesUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        provenance: UsageProvenance::ProviderReported,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use llm_proxy_core::AuthStyle;
    use llm_proxy_protocol::core::{CoreMessage, CoreTool, SamplingOptions};
    use llm_proxy_protocol::zen::ResponsesContent;

    fn make_target() -> ProviderAdapterTarget {
        ProviderAdapterTarget {
            provider_name: "test-responses".into(),
            adapter_name: "openai-responses".into(),
            protocol: super::super::ProviderProtocol::OpenAiResponses,
            endpoint: "https://api.openai.com/v1/responses".into(),
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

    // -- Encode tests --------------------------------------------------------

    #[test]
    fn encode_text_request() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "Hello".into(),
                cache: None,
            }],
        }]);
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.model, "gpt-4o-2024-08-06");
        assert_eq!(body.input.len(), 1);
        assert_eq!(body.input[0].role, "user");
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
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.input[0].role, "developer");
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
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].name, Some("get_weather".to_owned()));
    }

    #[test]
    fn encode_reasoning_effort() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.reasoning_effort = Some("high".into());
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert!(body.reasoning.is_some());
        assert_eq!(body.reasoning.unwrap().effort, Some("high".to_owned()));
    }

    #[test]
    fn encode_stream_flag() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.stream = true;
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.stream, Some(true));
        assert!(proxy_req.stream);
    }

    // -- Decode tests --------------------------------------------------------

    #[test]
    fn decode_text_response() {
        let target = make_target();
        let resp = ResponsesResponse {
            id: "resp_test".into(),
            object: "response".into(),
            created: 12345,
            model: "gpt-4o-2024-08-06".into(),
            output: vec![ResponsesOutput {
                r#type: "message".into(),
                id: Some("msg_1".into()),
                role: Some("assistant".into()),
                content: Some(vec![ResponsesContent {
                    r#type: "output_text".into(),
                    text: Some("hello world".into()),
                }]),
                call_id: None,
                name: None,
                arguments: None,
            }],
            usage: ResponsesUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.id, Some("resp_test".to_owned()));
        assert_eq!(core_resp.stop_reason, StopReason::EndTurn);
        assert_eq!(core_resp.usage.input_tokens, 100);
        assert_eq!(core_resp.content.len(), 1);
    }

    #[test]
    fn decode_function_call_response() {
        let target = make_target();
        let resp = ResponsesResponse {
            id: "resp_test".into(),
            object: "response".into(),
            created: 0,
            model: "gpt-4o".into(),
            output: vec![ResponsesOutput {
                r#type: "function_call".into(),
                id: Some("fc_1".into()),
                role: None,
                content: None,
                call_id: Some("call_123".into()),
                name: Some("get_weather".into()),
                arguments: Some(r#"{"city":"SF"}"#.into()),
            }],
            usage: ResponsesUsage {
                input_tokens: 200,
                output_tokens: 80,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.stop_reason, StopReason::ToolUse);
        match &core_resp.content[0] {
            CoreContent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn decode_preserves_requested_model() {
        let mut target = make_target();
        target.requested_model = "my-model".into();

        let resp = ResponsesResponse {
            id: "test".into(),
            object: "response".into(),
            created: 0,
            model: "gpt-4o-2024-08-06".into(),
            output: vec![ResponsesOutput {
                r#type: "message".into(),
                id: None,
                role: Some("assistant".into()),
                content: Some(vec![ResponsesContent {
                    r#type: "output_text".into(),
                    text: Some("hi".into()),
                }]),
                call_id: None,
                name: None,
                arguments: None,
            }],
            usage: ResponsesUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.model.requested, "my-model");
    }

    // -- Streaming tests -----------------------------------------------------

    #[test]
    fn stream_text_decoding() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(r#"{"type":"response.created","id":"resp_1"}"#),
            make_frame(r#"{"type":"response.output_text.delta","delta":"Hello"}"#),
            make_frame(r#"{"type":"response.output_text.delta","delta":" world"}"#),
            make_frame(
                r#"{"type":"response.completed","usage":{"input_tokens":10,"output_tokens":5}}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. }))
        );
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::ContentStart { .. }))
        );
        assert!(all_events.iter().any(|e| matches!(
            e,
            CoreEvent::TextDelta { text, .. } if text == "Hello"
        )));
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStop { .. }))
        );
    }

    #[test]
    fn stream_done_marker_ignored() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("[DONE]");
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn stream_finish_emits_lifecycle() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let events = decoder.finish().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStop { .. }))
        );
    }

    // -- Stream tool call / malformed frame / response.failed tests -----------

    #[test]
    fn stream_tool_call_decoding() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(r#"{"type":"response.created","id":"resp_1"}"#),
            make_frame(
                r#"{"type":"response.output_item.added","output":[{"type":"function_call","call_id":"call_abc","name":"get_weather"}]}"#,
            ),
            make_frame(r#"{"type":"response.function_call_arguments.delta","delta":"{\"city\":"}"#),
            make_frame(r#"{"type":"response.function_call_arguments.delta","delta":"\"SF\"}"}"#),
            make_frame(r#"{"type":"response.function_call_arguments.done"}"#),
            make_frame(
                r#"{"type":"response.completed","usage":{"input_tokens":20,"output_tokens":10}}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallStart { .. })),
            "expected ToolCallStart"
        );
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallDelta { .. })),
            "expected ToolCallDelta"
        );
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallStop { .. })),
            "expected ToolCallStop"
        );
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStop { .. })),
            "expected MessageStop with ToolUse"
        );
    }

    #[test]
    fn stream_parallel_tool_calls_partitioned_by_output_index() {
        // The Responses API interleaves parallel function-call argument deltas
        // and disambiguates them via `output_index`. The decoder must emit two
        // distinct ToolCallStart events with different indices and route each
        // delta to the correct content_index so the two argument streams are
        // partitioned rather than merged into one invalid JSON blob.
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(r#"{"type":"response.created","id":"resp_1"}"#),
            // First parallel call (output_index 0).
            make_frame(
                r#"{"type":"response.output_item.added","output_index":0,"output":[{"type":"function_call","call_id":"call_a","name":"get_weather"}]}"#,
            ),
            // Second parallel call (output_index 1) before the first completes.
            make_frame(
                r#"{"type":"response.output_item.added","output_index":1,"output":[{"type":"function_call","call_id":"call_b","name":"get_time"}]}"#,
            ),
            // Interleaved argument deltas.
            make_frame(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"city\":"}"#,
            ),
            make_frame(
                r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"tz\":"}"#,
            ),
            make_frame(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"SF\"}"}"#,
            ),
            make_frame(
                r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"UTC\"}"}"#,
            ),
            // Close the two calls out of arrival order (call_b first).
            make_frame(r#"{"type":"response.function_call_arguments.done","output_index":1}"#),
            make_frame(r#"{"type":"response.function_call_arguments.done","output_index":0}"#),
            make_frame(
                r#"{"type":"response.completed","usage":{"input_tokens":40,"output_tokens":20}}"#,
            ),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        // Two distinct ToolCallStart events with different indices.
        let starts: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                CoreEvent::ToolCallStart { index, id, name } => {
                    Some((*index, id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 2, "expected two ToolCallStart events");
        let indices: Vec<_> = starts.iter().map(|(i, _, _)| *i).collect();
        assert_eq!(indices, vec![0, 1], "expected distinct ascending indices");

        // The argument deltas must be partitioned per content_index, not merged.
        let mut args_by_index: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        for e in &all_events {
            if let CoreEvent::ToolCallDelta { index, args_delta } = e {
                *args_by_index.entry(*index).or_default() += args_delta;
            }
        }
        assert_eq!(
            &args_by_index[&0], r#"{"city":"SF"}"#,
            "output_index 0 args must be partitioned"
        );
        assert_eq!(
            &args_by_index[&1], r#"{"tz":"UTC"}"#,
            "output_index 1 args must be partitioned"
        );

        // Each started block gets a matching stop.
        let stops: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                CoreEvent::ToolCallStop { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(stops.len(), 2, "expected two ToolCallStop events");
        assert!(stops.contains(&0) && stops.contains(&1));
    }

    #[test]
    fn stream_malformed_frame_skipped() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("not valid json {{{");
        let events = decoder.decode_frame(&frame).unwrap();
        // Malformed frame should return empty (no MessageStart yet since type is unknown).
        assert!(events.is_empty());
    }

    #[test]
    fn stream_response_failed() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame =
            make_frame(r#"{"type":"response.failed","error":{"message":"rate limit exceeded"}}"#);
        let events = decoder.decode_frame(&frame).unwrap();

        assert!(
            events.iter().any(|e| matches!(e, CoreEvent::Error { .. })),
            "response.failed must emit CoreEvent::Error"
        );
    }

    // -- Encode: tool_choice forwarding test ----------------------------------

    #[test]
    fn encode_tool_choice_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.tool_choice = Some(CoreToolChoice::Auto);
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert!(body.tool_choice.is_some(), "tool_choice must be forwarded");
        let tc = body.tool_choice.unwrap();
        assert_eq!(tc["type"], "auto");
    }

    // -- Additional missing tests ----------------------------------------------

    #[test]
    fn encode_tool_use_as_function_call() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::Assistant,
            content: vec![CoreContent::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "SF"}),
            }],
        }]);
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.input.len(), 1);
        let content = body.input[0].content.as_ref().unwrap();
        assert_eq!(content["type"], "function_call");
        assert_eq!(content["name"], "get_weather");
    }

    #[test]
    fn encode_tool_result_as_function_call_output() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::Tool,
            content: vec![CoreContent::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![CoreContent::Text {
                    text: "72F sunny".into(),
                    cache: None,
                }],
                is_error: false,
            }],
        }]);
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.input.len(), 1);
        let content = body.input[0].content.as_ref().unwrap();
        assert_eq!(content["type"], "function_call_output");
        assert_eq!(content["call_id"], "call_1");
        assert_eq!(content["output"], "72F sunny");
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
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let params = body.tools[0].parameters.as_ref().unwrap();
        assert_eq!(params["type"], "object");
    }

    #[test]
    fn stream_stop_reason_mapping_tool_use() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(r#"{"type":"response.created","id":"resp_1"}"#),
            make_frame(
                r#"{"type":"response.output_item.added","output":[{"type":"function_call","call_id":"call_1","name":"test"}]}"#,
            ),
            make_frame(r#"{"type":"response.function_call_arguments.done"}"#),
            make_frame(r#"{"type":"response.done","usage":{"input_tokens":10,"output_tokens":5}}"#),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }

        // Stop reason should be ToolUse since we saw a function_call.
        let msg_stop = all_events
            .iter()
            .find(|e| matches!(e, CoreEvent::MessageStop { .. }));
        let Some(CoreEvent::MessageStop { stop_reason, .. }) = msg_stop else {
            panic!("expected a MessageStop event");
        };
        assert_eq!(*stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn stream_unknown_event_skipped() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(r#"{"type":"some_new_event","data":"whatever"}"#);
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn stream_finish_emits_tool_use_if_saw_tool_call() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        // Simulate receiving a tool call event but no explicit done/completed.
        let frames = vec![
            make_frame(r#"{"type":"response.created","id":"resp_1"}"#),
            make_frame(
                r#"{"type":"response.output_item.added","output":[{"type":"function_call","call_id":"c1","name":"fn"}]}"#,
            ),
            make_frame(r#"{"type":"response.function_call_arguments.done"}"#),
        ];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }
        all_events.extend(decoder.finish().unwrap());

        let msg_stop = all_events
            .iter()
            .find(|e| matches!(e, CoreEvent::MessageStop { .. }));
        let Some(CoreEvent::MessageStop { stop_reason, .. }) = msg_stop else {
            panic!("expected a MessageStop event");
        };
        assert_eq!(
            *stop_reason,
            StopReason::ToolUse,
            "finish() should infer ToolUse when saw_tool_call is true"
        );
    }

    #[test]
    fn decode_empty_content_gets_default_text() {
        let target = make_target();
        let resp = ResponsesResponse {
            id: "test".into(),
            object: "response".into(),
            created: 0,
            model: "gpt-4o".into(),
            output: vec![],
            usage: ResponsesUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.content.len(), 1);
        assert_eq!(
            core_resp.content[0],
            CoreContent::Text {
                text: String::new(),
                cache: None
            }
        );
    }

    #[test]
    fn encode_tool_choice_any_and_none() {
        for (tc, expected_type) in [
            (CoreToolChoice::Any, "required"),
            (CoreToolChoice::None, "none"),
        ] {
            let mut core = make_core_request(vec![CoreMessage {
                role: CoreRole::User,
                content: vec![CoreContent::Text {
                    text: "hi".into(),
                    cache: None,
                }],
            }]);
            core.tool_choice = Some(tc);
            let adapter = ResponsesAdapter;
            let target = make_target();
            let proxy_req = adapter.encode_request(&core, &target).unwrap();

            let body: ResponsesRequest = serde_json::from_slice(&proxy_req.body).unwrap();
            assert_eq!(body.tool_choice.unwrap()["type"], expected_type);
        }
    }

    // -- Additional tests: upstream model alias, malformed decode, etc --

    #[test]
    fn encode_uses_upstream_model() {
        let mut target = make_target();
        target.upstream_model = "gpt-4o-2024-08-06-alias".into();
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        let adapter = ResponsesAdapter;
        let proxy_req = adapter.encode_request(&core, &target).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body["model"], "gpt-4o-2024-08-06-alias");
    }

    #[test]
    fn encode_empty_messages() {
        let core = make_core_request(vec![]);
        let adapter = ResponsesAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&proxy_req.body).unwrap();
        // Should produce a valid request with empty input array.
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn decode_malformed_json_returns_error() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let result = adapter.decode_response(b"not valid json {{{", &target);
        assert!(result.is_err(), "malformed JSON should produce an error");
    }

    #[test]
    fn decode_empty_bytes_returns_error() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let result = adapter.decode_response(b"", &target);
        assert!(result.is_err(), "empty bytes should produce an error");
    }

    #[test]
    fn stream_response_failed_error_mapping() {
        let target = make_target();
        let adapter = ResponsesAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"type":"response.failed","error":{"code":"rate_limit_exceeded","message":"Too many requests"}}"#,
        );
        let events = decoder.decode_frame(&frame).unwrap();
        let error_event = events.iter().find(|e| matches!(e, CoreEvent::Error { .. }));
        assert!(
            error_event.is_some(),
            "response.failed should produce an Error event"
        );
        if let CoreEvent::Error { error } = error_event.unwrap() {
            assert_eq!(
                error.kind,
                llm_proxy_protocol::core::CoreStreamErrorKind::RateLimit
            );
        }
    }

    #[test]
    fn decode_status_failed_gives_error_stop_reason() {
        let target = make_target();
        let resp = ResponsesResponse {
            id: "resp_fail".into(),
            object: "response".into(),
            created: 0,
            model: "gpt-4o".into(),
            output: vec![],
            usage: ResponsesUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            status: Some("failed".into()),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();
        assert_eq!(core_resp.stop_reason, StopReason::Error);
    }

    #[test]
    fn decode_status_incomplete_gives_max_tokens_stop_reason() {
        let target = make_target();
        let resp = ResponsesResponse {
            id: "resp_inc".into(),
            object: "response".into(),
            created: 0,
            model: "gpt-4o".into(),
            output: vec![],
            usage: ResponsesUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            status: Some("incomplete".into()),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let adapter = ResponsesAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();
        assert_eq!(core_resp.stop_reason, StopReason::MaxTokens);
    }

    // -- Source guard ---------------------------------------------------------

    #[test]
    fn adapter_source_no_forbidden_imports() {
        let source = include_str!("responses.rs");
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

    fn make_frame(data: &str) -> SseFrame {
        SseFrame {
            event: None,
            id: None,
            data: data.to_owned(),
        }
    }
}
