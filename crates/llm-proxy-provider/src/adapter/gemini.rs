//! Google Gemini GenerateContent API provider adapter: translates core
//! requests, responses, and event streams to and from the Gemini wire format.

use std::collections::HashMap;

use llm_proxy_protocol::core::{
    ContentKind, CoreContent, CoreEvent, CoreRequest, CoreResponse, CoreRole, ModelRef, StopReason,
    Usage, UsageProvenance,
};
use llm_proxy_protocol::zen::{
    GeminiFunctionCall, GeminiFunctionDeclaration, GeminiGenerationConfig, GeminiPart,
    GeminiRequest, GeminiResponse, GeminiStreamChunk, GeminiTool, GeminiUsage,
};

use super::{
    ProviderAdapterTarget, ProviderStreamDecoder, build_proxy_request, expand_url_template,
    map_gemini_finish_reason, response_model_ref,
};
use crate::error::ProviderError;
use crate::sse::SseFrame;

/// Adapter for the Google Gemini GenerateContent API.
#[derive(Debug, Clone, Default)]
pub struct GeminiAdapter;

/// Stateful stream decoder translating Gemini SSE frames into `CoreEvent`s.
#[derive(Debug)]
pub struct GeminiStreamDecoder {
    model_ref: ModelRef,
    started: bool,
    content_index: usize,
    content_started: bool,
    tool_blocks: Vec<usize>,
    tool_blocks_closed: Vec<bool>,
    tool_id_counter: usize,
    stop_sent: bool,
}

impl ProviderStreamDecoder for GeminiStreamDecoder {
    fn decode_frame(&mut self, frame: &SseFrame) -> Result<Vec<CoreEvent>, ProviderError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(vec![]);
        }

        let chunk: GeminiStreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => {
                let truncated = super::truncate_str_safe(data, 200);
                tracing::warn!(data = truncated, "malformed Gemini chunk, skipping");
                return Ok(vec![]);
            }
        };

        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            events.push(CoreEvent::MessageStart {
                id: None,
                model: self.model_ref.clone(),
            });
        }

        if chunk.candidates.is_empty() {
            if let Some(ref usage) = chunk.usage_metadata {
                events.push(CoreEvent::UsageDelta {
                    usage: build_gemini_usage(usage),
                });
            }
            return Ok(events);
        }

        let candidate = &chunk.candidates[0];

        let parts: &[GeminiPart] = candidate
            .content
            .as_ref()
            .map(|c| c.parts.as_slice())
            .unwrap_or(&[]);
        for part in parts {
            if let Some(ref text) = part.text
                && !text.is_empty()
            {
                self.push_text_delta(text, &mut events);
            }

            if let Some(ref function_call) = part.function_call {
                self.push_function_call(function_call, &mut events);
            }
        }

        if let Some(ref reason) = candidate.finish_reason {
            if !reason.is_empty() && !self.stop_sent {
                self.close_content_if_open();

                let stop_reason = map_gemini_finish_reason(reason);

                if let Some(ref usage) = chunk.usage_metadata {
                    events.push(CoreEvent::UsageDelta {
                        usage: build_gemini_usage(usage),
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

        for (&idx, &closed) in self.tool_blocks.iter().zip(&self.tool_blocks_closed) {
            if !closed {
                events.push(CoreEvent::ToolCallStop { index: idx });
            }
        }

        if !self.stop_sent {
            self.stop_sent = true;
            events.push(CoreEvent::MessageStop {
                stop_reason: if !self.tool_blocks.is_empty() {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                stop_sequence: None,
            });
        }

        Ok(events)
    }
}

impl GeminiStreamDecoder {
    fn push_text_delta(&mut self, text: &str, events: &mut Vec<CoreEvent>) {
        if !self.content_started {
            self.content_started = true;
            events.push(CoreEvent::ContentStart {
                index: self.content_index,
                kind: ContentKind::Text,
            });
        }
        events.push(CoreEvent::TextDelta {
            index: self.content_index,
            text: text.to_owned(),
        });
    }

    fn push_function_call(
        &mut self,
        function_call: &GeminiFunctionCall,
        events: &mut Vec<CoreEvent>,
    ) {
        self.close_content_if_open();

        let block_idx = self.content_index;
        self.tool_blocks.push(block_idx);
        self.tool_blocks_closed.push(false);

        events.push(CoreEvent::ToolCallStart {
            index: block_idx,
            id: format!("gemini_call_{}", self.tool_id_counter),
            name: function_call.name.clone(),
        });
        self.tool_id_counter += 1;

        if let Some(ref args) = function_call.args {
            let args_str = serde_json::to_string(args).unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "Gemini: failed to serialize function_call args, falling back to empty object"
                );
                "{}".to_owned()
            });
            if args_str != "null" {
                events.push(CoreEvent::ToolCallDelta {
                    index: block_idx,
                    args_delta: args_str,
                });
            }
        }

        events.push(CoreEvent::ToolCallStop { index: block_idx });
        if let Some(last) = self.tool_blocks_closed.last_mut() {
            *last = true;
        }
        self.content_index += 1;
    }

    fn close_content_if_open(&mut self) {
        if self.content_started {
            self.content_started = false;
            self.content_index += 1;
        }
    }
}

fn encode_content_part(
    c: &CoreContent,
    tool_name_map: &HashMap<String, String>,
    parts: &mut Vec<GeminiPart>,
) -> Result<(), ProviderError> {
    match c {
        CoreContent::Text { text, cache } => {
            if cache.is_some() {
                tracing::warn!("Gemini: cache_control is not supported, dropping cache marker");
            }
            if !text.is_empty() {
                parts.push(GeminiPart::text(text.clone()));
            }
        }
        CoreContent::ToolUse { name, input, .. } => {
            parts.push(GeminiPart::function_call(name.clone(), Some(input.clone())));
        }
        CoreContent::ToolResult {
            tool_use_id,
            content: result_content,
            is_error,
        } => {
            if *is_error {
                tracing::warn!(
                    tool_use_id,
                    "Gemini: is_error flag in ToolResult is not representable in Gemini wire format and will be dropped"
                );
            }
            let response_val = encode_tool_result_payload(tool_use_id, result_content);

            let Some(fn_name) = tool_name_map.get(tool_use_id) else {
                return Err(ProviderError::InvalidConfig(format!(
                    "Gemini: cannot resolve function name for tool_use_id \
                     \"{tool_use_id}\"; the corresponding ToolUse block was \
                     not found in the conversation history"
                )));
            };
            parts.push(GeminiPart::function_response(fn_name.clone(), response_val));
        }
        _ => {
            tracing::warn!(
                ?c,
                "Gemini: dropping unsupported content type in message encoding"
            );
        }
    }
    Ok(())
}

fn encode_tool_result_payload(
    tool_use_id: &str,
    result_content: &[CoreContent],
) -> serde_json::Value {
    if result_content.is_empty() {
        return serde_json::json!({"result": ""});
    }
    let texts: Vec<&str> = result_content
        .iter()
        .filter_map(|rc| match rc {
            CoreContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if texts.len() != result_content.len() {
        tracing::warn!(
            tool_use_id,
            "Gemini: dropping non-text content in tool result"
        );
    }
    serde_json::json!({"result": texts.join("\n")})
}

impl GeminiAdapter {
    /// Encode a core request into a Gemini GenerateContent request.
    #[expect(
        clippy::too_many_lines,
        reason = "field-by-field wire encode of the Gemini GenerateContent request \
                 shape, kept in one place so the mapping can be audited against \
                 the API schema without jumping between helpers"
    )]
    pub fn encode_request(
        &self,
        core: &CoreRequest,
        target: &ProviderAdapterTarget,
    ) -> Result<super::ProxyRequest, ProviderError> {
        let mut contents = Vec::new();

        let tool_name_map = build_tool_name_map(core);

        for msg in &core.messages {
            let role = match msg.role {
                CoreRole::User => "user",
                CoreRole::Assistant => "model",
                CoreRole::System => "user",
                CoreRole::Tool => "user",
                _ => "user",
            };

            let mut parts: Vec<GeminiPart> = Vec::new();
            for c in &msg.content {
                encode_content_part(c, &tool_name_map, &mut parts)?;
            }

            if !parts.is_empty() {
                contents.push(llm_proxy_protocol::zen::GeminiContent {
                    role: role.to_owned(),
                    parts,
                });
            }
        }

        if !core.system.is_empty() {
            for c in &core.system {
                if !matches!(c, CoreContent::Text { .. }) {
                    tracing::warn!(
                        ?c,
                        "Gemini: dropping non-Text system content block during encode"
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
                contents.insert(
                    0,
                    llm_proxy_protocol::zen::GeminiContent {
                        role: "user".to_owned(),
                        parts: vec![GeminiPart::text(system_text)],
                    },
                );
                let next_is_model = contents.get(1).is_some_and(|c| c.role == "model");
                if !next_is_model {
                    contents.insert(
                        1,
                        llm_proxy_protocol::zen::GeminiContent {
                            role: "model".to_owned(),
                            parts: vec![GeminiPart::text("Understood.".to_owned())],
                        },
                    );
                }
            }
        }

        let tools: Vec<GeminiTool> = if core.tools.is_empty() {
            vec![]
        } else {
            vec![GeminiTool {
                function_declarations: core
                    .tools
                    .iter()
                    .map(|t| {
                        let schema = if !t.input_schema.is_object() {
                            serde_json::json!({"type": "object", "properties": {}})
                        } else {
                            t.input_schema.clone()
                        };
                        GeminiFunctionDeclaration {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: Some(schema),
                        }
                    })
                    .collect(),
            }]
        };

        let generation_config = GeminiGenerationConfig {
            temperature: core.sampling.temperature,
            top_p: core.sampling.top_p,
            max_output_tokens: core.sampling.max_tokens,
            stop_sequences: core
                .sampling
                .stop
                .as_ref()
                .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) }),
        };

        if core.tool_choice.is_some() {
            tracing::warn!(
                tool_choice = ?core.tool_choice,
                "Gemini: tool_choice specified but not yet forwarded to upstream; \
                 the model will use its default tool calling behavior"
            );
        }

        if core.sampling.reasoning_effort.is_some() {
            tracing::warn!(
                "Gemini: reasoning_effort specified but not forwarded; \
                 Gemini does not support this parameter"
            );
        }
        if core.metadata.user_id.is_some() {
            tracing::warn!(
                "Gemini: user_id specified but not forwarded; \
                 Gemini does not support this parameter"
            );
        }

        let req = GeminiRequest {
            contents,
            generation_config: Some(generation_config),
            tools,
            stream: if core.stream { Some(true) } else { None },
        };

        let body = serde_json::to_vec(&req)?;
        let url = expand_url_template(&target.endpoint, target)?;

        Ok(build_proxy_request(body, target, core.stream, url))
    }

    /// Decode a Gemini GenerateContent response body.
    pub fn decode_response(
        &self,
        bytes: &[u8],
        target: &ProviderAdapterTarget,
    ) -> Result<CoreResponse, ProviderError> {
        let resp: GeminiResponse = serde_json::from_slice(bytes)?;

        if resp.candidates.is_empty() {
            return Err(ProviderError::EmptyResponse(
                "no candidates in response".to_owned(),
            ));
        }

        let candidate = &resp.candidates[0];
        let mut content = Vec::new();
        let mut tool_id_counter = 0usize;

        let parts: &[GeminiPart] = candidate
            .content
            .as_ref()
            .map(|c| c.parts.as_slice())
            .unwrap_or(&[]);
        for part in parts {
            if let Some(ref text) = part.text {
                if !text.is_empty() {
                    content.push(CoreContent::Text {
                        text: text.clone(),
                        cache: None,
                    });
                }
            }
            if let Some(ref function_call) = part.function_call {
                let input = function_call
                    .args
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                content.push(CoreContent::ToolUse {
                    id: format!("gemini_call_{}", tool_id_counter),
                    name: function_call.name.clone(),
                    input,
                });
                tool_id_counter += 1;
            }
        }

        if content.is_empty() {
            content.push(CoreContent::Text {
                text: String::new(),
                cache: None,
            });
        }

        let has_tool_use = content
            .iter()
            .any(|c| matches!(c, CoreContent::ToolUse { .. }));
        let stop_reason = candidate
            .finish_reason
            .as_deref()
            .map(map_gemini_finish_reason)
            .unwrap_or(if has_tool_use {
                StopReason::ToolUse
            } else {
                StopReason::Unknown("absent".to_owned())
            });

        let usage = resp
            .usage_metadata
            .as_ref()
            .map(build_gemini_usage)
            .unwrap_or_else(Usage::synthetic_zero);

        Ok(CoreResponse {
            id: None,
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
    pub fn new_stream_decoder(&self, target: &ProviderAdapterTarget) -> GeminiStreamDecoder {
        GeminiStreamDecoder {
            model_ref: response_model_ref(target),
            started: false,
            content_index: 0,
            content_started: false,
            tool_blocks: Vec::new(),
            tool_blocks_closed: Vec::new(),
            tool_id_counter: 0,
            stop_sent: false,
        }
    }
}

fn build_tool_name_map(core: &CoreRequest) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in &core.messages {
        for content in &msg.content {
            if let CoreContent::ToolUse { id, name, .. } = content {
                map.entry(id.clone()).or_insert_with(|| name.clone());
            }
        }
    }
    map
}

fn build_gemini_usage(usage: &GeminiUsage) -> Usage {
    Usage {
        input_tokens: usage.prompt_token_count,
        output_tokens: usage.candidates_token_count,
        reasoning_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        provenance: UsageProvenance::ProviderReported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_proxy_protocol::core::{CoreMessage, CoreTool, SamplingOptions};

    fn make_target() -> ProviderAdapterTarget {
        ProviderAdapterTarget {
            provider_name: "test-gemini".into(),
            adapter_name: "gemini".into(),
            protocol: super::super::ProviderProtocol::GeminiGenerateContent,
            endpoint:
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                    .into(),
            auth_style: llm_proxy_core::AuthStyle::Bearer,
            api_key: secrecy::SecretString::from("test-key"),
            requested_model: "gemini-2.5-pro".into(),
            upstream_model: "gemini-2.5-pro".into(),
            headers: std::sync::Arc::new(std::collections::HashMap::new()),
        }
    }

    fn make_core_request(messages: Vec<CoreMessage>) -> CoreRequest {
        CoreRequest {
            model: ModelRef {
                requested: "gemini-2.5-pro".into(),
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

    #[test]
    fn encode_text_request() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "Hello".into(),
                cache: None,
            }],
        }]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[0].parts[0].text, Some("Hello".to_owned()));
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
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(
            body.contents[0].parts[0].text,
            Some("You are helpful".to_owned())
        );
        assert_eq!(body.contents[1].role, "model");
    }

    #[test]
    fn encode_system_prompt_no_double_model_when_assistant_first() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::Assistant,
            content: vec![CoreContent::Text {
                text: "I will help.".into(),
                cache: None,
            }],
        }]);
        core.system = vec![CoreContent::Text {
            text: "You are helpful".into(),
            cache: None,
        }];
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[1].role, "model");
        let has_understood = body.contents.iter().any(|c| {
            c.parts
                .iter()
                .any(|p| p.text.as_deref() == Some("Understood."))
        });
        assert!(
            !has_understood,
            "synthetic Understood. turn must be suppressed when the next turn is already model"
        );
        for pair in body.contents.windows(2) {
            assert!(
                !(pair[0].role == "model" && pair[1].role == "model"),
                "consecutive model turns are not allowed: {:?} -> {:?}",
                pair[0].role,
                pair[1].role
            );
        }
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
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].function_declarations[0].name, "get_weather");
    }

    #[test]
    fn encode_generation_config() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling = SamplingOptions {
            temperature: Some(0.7),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let config = body.generation_config.unwrap();
        assert_eq!(config.temperature, Some(0.7));
        assert_eq!(config.max_output_tokens, Some(1024));
    }

    #[test]
    fn encode_url_template_expansion() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        assert_eq!(
            proxy_req.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn decode_text_response() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "hello world"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.model.requested, "gemini-2.5-pro");
        assert_eq!(core_resp.stop_reason, StopReason::EndTurn);
        assert_eq!(core_resp.usage.input_tokens, 100);
        assert_eq!(core_resp.usage.output_tokens, 50);
        assert_eq!(core_resp.content.len(), 1);
    }

    #[test]
    fn decode_function_call_response() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"function_call": {"name": "get_weather", "args": {"city": "SF"}}}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 200,
                "candidatesTokenCount": 80,
                "totalTokenCount": 280
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        match &core_resp.content[0] {
            CoreContent::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn decode_no_candidates_error() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [],
            "usageMetadata": null
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let result = adapter.decode_response(&bytes, &target);
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::EmptyResponse(_) => {}
            other => panic!("expected EmptyResponse, got {:?}", other),
        }
    }

    #[test]
    fn decode_preserves_requested_model() {
        let mut target = make_target();
        target.requested_model = "my-gemini".into();

        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "hi"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.model.requested, "my-gemini");
    }

    #[test]
    fn stream_text_decoding() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":null}],"usageMetadata":null}"#,
            ),
            make_frame(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
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
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("[DONE]");
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn stream_finish_emits_lifecycle() {
        let target = make_target();
        let adapter = GeminiAdapter;
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

    #[test]
    fn stream_tool_call_decoding() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"function_call":{"name":"get_weather","args":{"city":"SF"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        );

        let events = decoder.decode_frame(&frame).unwrap();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallStart { .. })),
            "expected ToolCallStart"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallDelta { .. })),
            "expected ToolCallDelta"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCallStop { .. })),
            "expected ToolCallStop"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStop { .. })),
            "expected MessageStop"
        );
    }

    #[test]
    fn stream_malformed_frame_skipped() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame("not valid json {{{");
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(
            events.is_empty(),
            "malformed frame should produce no events"
        );
    }

    #[test]
    fn stream_usage_only_chunk() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"candidates":[],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"totalTokenCount":150}}"#,
        );
        let events = decoder.decode_frame(&frame).unwrap();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::UsageDelta { .. })),
            "usage-only chunk must emit UsageDelta"
        );
    }

    #[test]
    fn encode_tool_result_as_function_response() {
        let core = make_core_request(vec![
            CoreMessage {
                role: CoreRole::Assistant,
                content: vec![CoreContent::ToolUse {
                    id: "gemini_call_0".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "SF"}),
                }],
            },
            CoreMessage {
                role: CoreRole::Tool,
                content: vec![CoreContent::ToolResult {
                    tool_use_id: "gemini_call_0".into(),
                    content: vec![CoreContent::Text {
                        text: "72F, sunny".into(),
                        cache: None,
                    }],
                    is_error: false,
                }],
            },
        ]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let fr = body
            .contents
            .iter()
            .find_map(|c| c.parts.iter().find_map(|p| p.function_response.clone()))
            .expect("ToolResult must be encoded as a functionResponse");
        assert_eq!(fr.name, "get_weather");
    }

    #[test]
    fn encode_tool_use_as_function_call() {
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::Assistant,
            content: vec![CoreContent::ToolUse {
                id: "gemini_call_0".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "SF"}),
            }],
        }]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body.contents.len(), 1);
        let part = &body.contents[0].parts[0];
        assert!(
            part.function_call.is_some(),
            "ToolUse must be encoded as functionCall"
        );
        let fc = part.function_call.as_ref().unwrap();
        assert_eq!(fc.name, "get_weather");
    }

    #[test]
    fn encode_top_p_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.top_p = Some(0.9);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let config = body.generation_config.unwrap();
        assert_eq!(config.top_p, Some(0.9));
    }

    #[test]
    fn encode_input_schema_coercion() {
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
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let params = body.tools[0].function_declarations[0]
            .parameters
            .as_ref()
            .unwrap();
        assert_eq!(params["type"], "object");
    }

    #[test]
    fn usage_mapping() {
        let usage = GeminiUsage {
            prompt_token_count: 100,
            candidates_token_count: 50,
            total_token_count: 150,
        };
        let core_usage = build_gemini_usage(&usage);
        assert_eq!(core_usage.input_tokens, 100);
        assert_eq!(core_usage.output_tokens, 50);
        assert_eq!(core_usage.provenance, UsageProvenance::ProviderReported);
    }

    #[test]
    fn stream_full_lifecycle_ordering() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![
            make_frame(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":null}],"usageMetadata":null}"#,
            ),
            make_frame(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
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
    fn encode_stop_sequences_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.stop = Some(vec!["END".into(), "STOP".into()]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let config = body.generation_config.unwrap();
        assert_eq!(
            config.stop_sequences,
            Some(vec!["END".into(), "STOP".into()])
        );
    }

    #[test]
    fn encode_tool_result_lookup_function_name() {
        let core = make_core_request(vec![
            CoreMessage {
                role: CoreRole::Assistant,
                content: vec![CoreContent::ToolUse {
                    id: "gemini_call_0".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "SF"}),
                }],
            },
            CoreMessage {
                role: CoreRole::Tool,
                content: vec![CoreContent::ToolResult {
                    tool_use_id: "gemini_call_0".into(),
                    content: vec![CoreContent::Text {
                        text: "72F".into(),
                        cache: None,
                    }],
                    is_error: false,
                }],
            },
        ]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let fr = body
            .contents
            .iter()
            .find_map(|c| c.parts.iter().find_map(|p| p.function_response.clone()));
        assert!(fr.is_some(), "expected function_response part");
        assert_eq!(fr.unwrap().name, "get_weather");
    }

    #[test]
    fn stream_unknown_event_skipped() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(r#"{"candidates":[],"usageMetadata":null}"#);
        let events = decoder.decode_frame(&frame).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::MessageStart { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoreEvent::TextDelta { .. }))
        );
    }

    #[test]
    fn decode_stop_reason_max_tokens() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "truncated"}]
                },
                "finishReason": "MAX_TOKENS"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        assert_eq!(core_resp.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn decode_empty_content_gets_default_text() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": []
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
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
    fn stream_finish_emits_tool_use_for_unclosed_blocks() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frame = make_frame(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"function_call":{"name":"get_weather","args":{"city":"SF"}}}]},"finishReason":null}],"usageMetadata":null}"#,
        );
        decoder.decode_frame(&frame).unwrap();

        let events = decoder.finish().unwrap();
        let msg_stop = events
            .iter()
            .find(|e| matches!(e, CoreEvent::MessageStop { .. }));
        let Some(CoreEvent::MessageStop { stop_reason, .. }) = msg_stop else {
            panic!("expected a MessageStop event");
        };
        assert_eq!(*stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn encode_reasoning_effort_omitted() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.sampling.reasoning_effort = Some("high".into());
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();

        let body: GeminiRequest = serde_json::from_slice(&proxy_req.body).unwrap();
        let config = body.generation_config.unwrap();
        assert_eq!(config.temperature, None);
    }

    #[test]
    fn decode_function_call_tool_use_id_format() {
        let target = make_target();
        let resp_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"function_call": {"name": "search", "args": {"q": "test"}}}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });
        let bytes = serde_json::to_vec(&resp_json).unwrap();
        let adapter = GeminiAdapter;
        let core_resp = adapter.decode_response(&bytes, &target).unwrap();

        match &core_resp.content[0] {
            CoreContent::ToolUse { id, name, .. } => {
                assert!(
                    id.starts_with("gemini_call_"),
                    "Gemini tool use IDs should start with gemini_call_"
                );
                assert_eq!(name, "search");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn encode_uses_upstream_model() {
        let mut target = make_target();
        target.upstream_model = "gemini-2.5-flash-preview-05-20".into();
        let core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        let adapter = GeminiAdapter;
        let proxy_req = adapter.encode_request(&core, &target).unwrap();
        assert!(proxy_req.url.contains("gemini-2.5-flash-preview-05-20"));
    }

    #[test]
    fn encode_stream_flag_forwarded() {
        let mut core = make_core_request(vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "hi".into(),
                cache: None,
            }],
        }]);
        core.stream = true;
        let target = make_target();
        let adapter = GeminiAdapter;
        let proxy_req = adapter.encode_request(&core, &target).unwrap();
        assert!(
            proxy_req.stream,
            "stream flag should be forwarded to ProxyRequest"
        );
    }

    #[test]
    fn encode_empty_messages() {
        let core = make_core_request(vec![]);
        let adapter = GeminiAdapter;
        let target = make_target();
        let proxy_req = adapter.encode_request(&core, &target).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&proxy_req.body).unwrap();
        assert_eq!(body["contents"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn decode_malformed_json_returns_error() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let result = adapter.decode_response(b"not valid json {{{", &target);
        assert!(result.is_err(), "malformed JSON should produce an error");
    }

    #[test]
    fn decode_empty_bytes_returns_error() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let result = adapter.decode_response(b"", &target);
        assert!(result.is_err(), "empty bytes should produce an error");
    }

    #[test]
    fn stream_no_duplicate_tool_call_stop() {
        let target = make_target();
        let adapter = GeminiAdapter;
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = vec![make_frame(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"function_call":{"name":"search","args":{"q":"rust"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        )];

        let mut all_events = Vec::new();
        for frame in &frames {
            all_events.extend(decoder.decode_frame(frame).unwrap());
        }
        all_events.extend(decoder.finish().unwrap());

        let tool_call_stop_count = all_events
            .iter()
            .filter(|e| matches!(e, CoreEvent::ToolCallStop { .. }))
            .count();
        assert_eq!(
            tool_call_stop_count, 1,
            "ToolCallStop should be emitted exactly once"
        );
    }

    #[test]
    fn adapter_source_no_forbidden_imports() {
        let source = include_str!("gemini.rs");
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

    fn make_frame(data: &str) -> SseFrame {
        SseFrame {
            event: None,
            id: None,
            data: data.to_owned(),
        }
    }
}
