//! Fixture-driven integration tests for provider protocol adapters.
//!
//! Loads each fixture's input JSON, runs encode/decode against the adapter,
//! and compares the result with the expected output JSON. For stream fixtures,
//! parses SSE input, feeds through the decoder, and compares events with
//! core-events.json.

#[cfg(test)]
mod tests {

    use llm_proxy_core::AuthStyle;
    use llm_proxy_protocol::core::{
        CacheControl, CacheControlType, CoreContent, CoreEvent, CoreMessage, CoreRequest, CoreRole,
        CoreTool, CoreToolChoice, ModelRef, SamplingOptions,
    };
    use llm_proxy_provider::{
        AnthropicAdapter, GeminiAdapter, OpenAiChatAdapter, ProviderAdapter, ProviderAdapterTarget,
        ProviderProtocol, ProviderStreamDecoder, ResponsesAdapter, SseFrame,
    };

    use std::fs;
    use std::path::Path;

    // ---------------------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------------------

    const FIXTURE_ROOT: &str = "tests/fixtures";

    // ---------------------------------------------------------------------------
    // Adapter names
    // ---------------------------------------------------------------------------

    const OPENAI_CHAT: &str = "openai_chat";
    const ANTHROPIC: &str = "anthropic";
    const RESPONSES: &str = "responses";
    const GEMINI: &str = "gemini";

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    /// Read a fixture file as a parsed JSON value.
    fn read_fixture(dir: &Path, name: &str) -> serde_json::Value {
        let path = dir.join(name);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse {} as JSON: {e}", path.display()))
    }

    /// Read a fixture file as raw string.
    fn read_fixture_raw(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
    }

    /// Build a default ProviderAdapterTarget for the given protocol.
    fn make_target(protocol: ProviderProtocol) -> ProviderAdapterTarget {
        let (endpoint, auth_style, model): (String, AuthStyle, String) = match protocol {
            ProviderProtocol::OpenAiChatCompletions => (
                "https://api.openai.com/v1/chat/completions".into(),
                AuthStyle::Bearer,
                "gpt-4o".into(),
            ),
            ProviderProtocol::AnthropicMessages => (
                "https://api.anthropic.com/v1/messages".into(),
                AuthStyle::XApiKey,
                "claude-sonnet-4-20250514".into(),
            ),
            ProviderProtocol::OpenAiResponses => (
                "https://api.openai.com/v1/responses".into(),
                AuthStyle::Bearer,
                "gpt-4o".into(),
            ),
            ProviderProtocol::GeminiGenerateContent => (
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                    .into(),
                AuthStyle::Bearer,
                "gemini-2.5-pro".into(),
            ),
            _ => (
                "https://example.com".into(),
                AuthStyle::Bearer,
                "model".into(),
            ),
        };
        ProviderAdapterTarget {
            provider_name: "test".into(),
            adapter_name: protocol.name().into(),
            protocol,
            endpoint,
            auth_style,
            api_key: secrecy::SecretString::from("test-key"),
            requested_model: model.clone(),
            upstream_model: model,
            headers: std::sync::Arc::new(std::collections::HashMap::new()),
        }
    }

    /// Get the ProviderAdapter for the given name.
    fn get_adapter(name: &str) -> ProviderAdapter {
        match name {
            OPENAI_CHAT => ProviderAdapter::OpenAiChat(OpenAiChatAdapter),
            ANTHROPIC => ProviderAdapter::Anthropic(AnthropicAdapter::new()),
            RESPONSES => ProviderAdapter::Responses(ResponsesAdapter),
            GEMINI => ProviderAdapter::Gemini(GeminiAdapter),
            _ => panic!("unknown adapter: {name}"),
        }
    }

    /// Get the protocol for the given adapter name.
    fn get_protocol(name: &str) -> ProviderProtocol {
        match name {
            OPENAI_CHAT => ProviderProtocol::OpenAiChatCompletions,
            ANTHROPIC => ProviderProtocol::AnthropicMessages,
            RESPONSES => ProviderProtocol::OpenAiResponses,
            GEMINI => ProviderProtocol::GeminiGenerateContent,
            _ => panic!("unknown adapter: {name}"),
        }
    }

    /// Build a CoreRequest from the human-readable core.json fixture format.
    ///
    /// The fixture format uses shorthand conventions:
    /// - `"model": "gpt-4o"` -> `ModelRef { requested: "gpt-4o", upstream: None }`
    /// - `"tool_choice": "auto"` -> `CoreToolChoice::Auto`
    /// - `"tool_choice": null` -> `None`
    fn build_core_request(core_json: &serde_json::Value) -> CoreRequest {
        let obj = core_json
            .as_object()
            .expect("core.json must be a JSON object");

        // Model
        let model = match obj.get("model") {
            Some(v) if v.is_string() => ModelRef {
                requested: v.as_str().unwrap().to_owned(),
                upstream: None,
            },
            Some(v) if v.is_object() => {
                serde_json::from_value(v.clone()).expect("model as object must be valid ModelRef")
            }
            _ => panic!("core.json missing 'model' field"),
        };

        // System
        let system: Vec<CoreContent> = obj
            .get("system")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_core_content).collect())
            .unwrap_or_default();

        // Messages
        let messages: Vec<CoreMessage> = obj
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_core_message).collect())
            .unwrap_or_default();

        // Tools
        let tools: Vec<CoreTool> = obj
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_core_tool).collect())
            .unwrap_or_default();

        // Tool choice
        let tool_choice = obj.get("tool_choice").and_then(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => Some(parse_tool_choice_string(s)),
            serde_json::Value::Object(_) => {
                Some(serde_json::from_value(v.clone()).expect("invalid tool_choice object"))
            }
            _ => None,
        });

        // Sampling
        let sampling: SamplingOptions = obj.get("sampling").map(parse_sampling).unwrap_or_default();

        // Stream
        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

        CoreRequest {
            model,
            system,
            messages,
            tools,
            tool_choice,
            sampling,
            stream,
            metadata: Default::default(),
            provider_hints: Default::default(),
        }
    }

    /// Parse a CoreContent from JSON.
    fn parse_core_content(v: &serde_json::Value) -> CoreContent {
        let obj = v.as_object().expect("content must be a JSON object");
        match obj.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                let cache = obj.get("cache").and_then(|v| {
                    let ctrl = v.as_object()?;
                    let typ = ctrl.get("type")?.as_str()?;
                    match typ {
                        "ephemeral" => Some(CacheControl {
                            r#type: CacheControlType::Ephemeral,
                        }),
                        other => panic!(
                            "parse_core_content: unknown cache control type '{other}' in fixture. \
                         Use 'ephemeral' or add a branch for this type."
                        ),
                    }
                });
                CoreContent::Text {
                    text: obj
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_owned(),
                    cache,
                }
            }
            "tool_use" => CoreContent::ToolUse {
                id: obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                input: obj.get("input").cloned().unwrap_or(serde_json::json!({})),
            },
            "tool_result" => CoreContent::ToolResult {
                tool_use_id: obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                is_error: obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                content: obj
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(parse_core_content).collect())
                    .unwrap_or_default(),
            },
            "thinking" => CoreContent::Thinking {
                text: obj
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_owned(),
                signature: obj
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            },
            "redacted_thinking" => CoreContent::RedactedThinking {
                data: obj.get("data").cloned().unwrap_or(serde_json::json!({})),
            },
            "image" => CoreContent::Image {
                source: obj.get("source").cloned().unwrap_or(serde_json::json!({})),
            },
            "document" => CoreContent::Document {
                source: obj.get("source").cloned().unwrap_or(serde_json::json!({})),
            },
            "audio" => CoreContent::Audio {
                source: obj.get("source").cloned().unwrap_or(serde_json::json!({})),
            },
            "video" => CoreContent::Video {
                source: obj.get("source").cloned().unwrap_or(serde_json::json!({})),
            },
            "refusal" => CoreContent::Refusal {
                text: obj
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_owned(),
            },
            other => panic!(
                "parse_core_content: unknown content type '{other}' in fixture. \
             Add a branch for this type."
            ),
        }
    }

    /// Parse a CoreMessage from JSON.
    fn parse_core_message(v: &serde_json::Value) -> CoreMessage {
        let obj = v.as_object().expect("message must be a JSON object");
        let role = match obj.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
            "user" => CoreRole::User,
            "assistant" => CoreRole::Assistant,
            "system" => CoreRole::System,
            "tool" => CoreRole::Tool,
            other => panic!(
                "parse_core_message: unknown role '{other}' in fixture. \
             Use 'user', 'assistant', 'system', or 'tool'."
            ),
        };
        let content: Vec<CoreContent> = obj
            .get("content")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_core_content).collect())
            .unwrap_or_default();
        CoreMessage { role, content }
    }

    /// Parse a CoreTool from JSON.
    fn parse_core_tool(v: &serde_json::Value) -> CoreTool {
        let obj = v.as_object().expect("tool must be a JSON object");
        CoreTool {
            name: obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
            description: obj
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned()),
            input_schema: obj
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({})),
        }
    }

    /// Parse a tool_choice string value.
    fn parse_tool_choice_string(s: &str) -> CoreToolChoice {
        match s {
            "auto" => CoreToolChoice::Auto,
            "any" | "required" => CoreToolChoice::Any,
            "none" => CoreToolChoice::None,
            _ => CoreToolChoice::Auto,
        }
    }

    /// Parse SamplingOptions from JSON.
    fn parse_sampling(v: &serde_json::Value) -> SamplingOptions {
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("sampling must be a JSON object"));
        SamplingOptions {
            temperature: obj
                .get("temperature")
                .and_then(|v| if v.is_null() { None } else { v.as_f64() }),
            top_p: obj
                .get("top_p")
                .and_then(|v| if v.is_null() { None } else { v.as_f64() }),
            max_tokens: obj.get("max_tokens").and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_i64().map(|i| i as i32)
                }
            }),
            stop: obj.get("stop").and_then(|v| {
                if v.is_null() {
                    None
                } else if v.is_string() {
                    Some(vec![v.as_str().unwrap().to_owned()])
                } else if v.is_array() {
                    Some(
                        v.as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_owned()))
                            .collect(),
                    )
                } else {
                    None
                }
            }),
            reasoning_effort: obj
                .get("reasoning_effort")
                .and_then(|v| v.as_str().map(|s| s.to_owned())),
            thinking: obj.get("thinking").cloned(),
        }
    }

    /// Parse SSE text into a list of SseFrame values.
    fn parse_sse_frames(sse_text: &str) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        let mut current_data = String::new();
        let mut current_event: Option<String> = None;

        for line in sse_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // End of event
                if !current_data.is_empty() {
                    frames.push(SseFrame {
                        event: current_event.take(),
                        id: None,
                        data: current_data.trim_end().to_owned(),
                    });
                    current_data.clear();
                }
            } else if let Some(data) = trimmed.strip_prefix("data:") {
                if !current_data.is_empty() {
                    current_data.push('\n');
                }
                current_data.push_str(data.trim_start());
            } else if let Some(event) = trimmed.strip_prefix("event:") {
                current_event = Some(event.trim_start().to_owned());
            }
            // Ignore other SSE fields (id:, retry:, comments)
        }

        // Handle last event if file doesn't end with blank line
        if !current_data.is_empty() {
            frames.push(SseFrame {
                event: current_event,
                id: None,
                data: current_data.trim_end().to_owned(),
            });
        }

        frames
    }

    /// Normalize a stop reason string to PascalCase for comparison.
    /// Accepts both snake_case ("end_turn") and PascalCase ("EndTurn").
    fn normalize_stop_reason(s: &str) -> &'static str {
        match s {
            "end_turn" | "EndTurn" => "EndTurn",
            "max_tokens" | "MaxTokens" => "MaxTokens",
            "tool_use" | "ToolUse" => "ToolUse",
            "stop_sequence" | "StopSequence" => "StopSequence",
            "refusal" | "Refusal" => "Refusal",
            "error" | "Error" => "Error",
            "unknown" | "Unknown" => "Unknown",
            _ => "Unknown",
        }
    }

    /// Extract the variant key from an externally-tagged CoreContent JSON value.
    /// E.g., `{"Text": {...}}` returns "Text".
    fn extract_content_variant(v: &serde_json::Value) -> String {
        v.as_object()
            .and_then(|obj| {
                obj.keys()
                    .find(|k| {
                        matches!(
                            k.as_str(),
                            "Text"
                                | "ToolUse"
                                | "ToolResult"
                                | "Thinking"
                                | "RedactedThinking"
                                | "Image"
                                | "Document"
                                | "Audio"
                                | "Video"
                                | "Refusal"
                        )
                    })
                    .cloned()
            })
            .unwrap_or_else(|| "Unknown".to_owned())
    }

    // ---------------------------------------------------------------------------
    // Provider request encode tests
    // ---------------------------------------------------------------------------

    /// Test that encoding a CoreRequest produces valid provider wire format.
    ///
    /// Verifies the adapter successfully encodes the request and the output
    /// matches key fields from output.json.  The output.json fixture serves
    /// both as documentation and as a structural comparison target.
    /// Flattens a wire `content` field to plain text.
    ///
    /// Provider formats allow either a bare string or an array of content
    /// blocks; only `text` blocks contribute.
    fn json_text_content(v: &serde_json::Value) -> String {
        if let Some(text) = v.as_str() {
            return text.to_owned();
        }
        let Some(blocks) = v.as_array() else {
            return String::new();
        };
        blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// Asserts one encoded message matches the fixture's expected message.
    fn assert_encoded_message(
        adapter_name: &str,
        case: &str,
        i: usize,
        act_msg: &serde_json::Value,
        exp_msg: &serde_json::Value,
    ) {
        assert_eq!(
            act_msg.get("role"),
            exp_msg.get("role"),
            "{adapter_name}/{case}: messages[{i}].role mismatch"
        );

        if let Some(exp_content) = exp_msg.get("content") {
            let Some(act_content) = act_msg.get("content") else {
                panic!("{adapter_name}/{case}: messages[{i}] missing content field");
            };
            // A string expectation accepts either a matching string or an array
            // whose text blocks concatenate to the same string; anything else
            // must match exactly.
            if exp_content.is_string() {
                assert_eq!(
                    json_text_content(act_content),
                    exp_content.as_str().unwrap_or(""),
                    "{adapter_name}/{case}: messages[{i}].content text mismatch"
                );
            } else {
                assert_eq!(
                    act_content, exp_content,
                    "{adapter_name}/{case}: messages[{i}].content mismatch"
                );
            }
        }

        let Some(exp_tcs) = exp_msg.get("tool_calls").and_then(|v| v.as_array()) else {
            return;
        };
        let act_tcs = act_msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map_or(&[][..], |a| a.as_slice());
        assert_eq!(
            act_tcs.len(),
            exp_tcs.len(),
            "{adapter_name}/{case}: messages[{i}].tool_calls count mismatch"
        );
        for (j, exp_tc) in exp_tcs.iter().enumerate() {
            let act_tc = &act_tcs[j];
            if exp_tc.get("id").is_some() {
                assert_eq!(
                    act_tc.get("id"),
                    exp_tc.get("id"),
                    "{adapter_name}/{case}: messages[{i}].tool_calls[{j}].id mismatch"
                );
            }
            if let Some(exp_fn) = exp_tc.get("function")
                && let Some(act_fn) = act_tc.get("function")
            {
                assert_eq!(
                    act_fn.get("name"),
                    exp_fn.get("name"),
                    "{adapter_name}/{case}: messages[{i}].tool_calls[{j}].function.name mismatch"
                );
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "walks one fixture's expected wire output field by field \
                 (messages, tools, sampling); splitting the walk would scatter \
                 the fixture's assertions across helpers"
    )]
    fn run_encode_request_fixture(adapter_name: &str, case: &str) {
        let dir = Path::new(FIXTURE_ROOT).join(adapter_name).join(case);
        let core_json = read_fixture(&dir, "core.json");
        let output_json = read_fixture(&dir, "output.json");

        let core = build_core_request(&core_json);
        let protocol = get_protocol(adapter_name);
        let target = make_target(protocol);
        let adapter = get_adapter(adapter_name);

        let proxy_req = adapter
            .encode_request(&core, &target)
            .unwrap_or_else(|e| panic!("encode_request failed for {adapter_name}/{case}: {e}"));

        let actual: serde_json::Value =
            serde_json::from_slice(&proxy_req.body).unwrap_or_else(|e| {
                panic!("encoded body is not valid JSON for {adapter_name}/{case}: {e}")
            });

        // The adapter must produce a JSON object.
        assert!(
            actual.is_object(),
            "{adapter_name}/{case}: encoded body is not a JSON object"
        );

        // Adapter must include the target model somewhere -- either in the request
        // body or in the URL (Gemini puts the model in the URL path).
        let model_str = target.upstream_model.as_str();
        let body_str = actual.to_string();
        let has_model = body_str.contains(model_str) || proxy_req.url.contains(model_str);
        assert!(
            has_model,
            "{adapter_name}/{case}: encoded request does not contain model '{model_str}' in body or URL\nbody: {actual:#?}\nurl: {}",
            proxy_req.url
        );

        // Compare key structural fields from output.json against the encoded body.
        // Messages/system/tools/sampling fields are compared when present in output.json.
        if let Some(expected_msgs) = output_json.get("messages").and_then(|v| v.as_array()) {
            let actual_msgs = actual.get("messages").and_then(|v| v.as_array());
            let actual_msgs = actual_msgs.map(|a| a.as_slice()).unwrap_or(&[]);
            assert_eq!(
                actual_msgs.len(),
                expected_msgs.len(),
                "{adapter_name}/{case}: message count mismatch (encoded={}, expected={})",
                actual_msgs.len(),
                expected_msgs.len()
            );
            for (i, exp_msg) in expected_msgs.iter().enumerate() {
                assert_encoded_message(adapter_name, case, i, &actual_msgs[i], exp_msg);
            }
        }

        // Compare system prompt content if present in output.json.
        if let Some(expected_sys) = output_json.get("system") {
            assert_eq!(
                actual.get("system"),
                Some(expected_sys),
                "{adapter_name}/{case}: system prompt mismatch"
            );
        }

        // Compare model field if present in output.json.
        // Verify that the encoded body or URL contains a model name. The exact
        // value comes from the test target's upstream_model, which may differ
        // from the fixture's expected model, so we only check presence.
        if output_json.get("model").and_then(|v| v.as_str()).is_some() {
            // Model presence already verified by the has_model assertion above.
        }

        // Compare tools count and key fields if present in output.json.
        if let Some(expected_tools) = output_json.get("tools").and_then(|v| v.as_array()) {
            let actual_tools = actual
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|a| a.as_slice())
                .unwrap_or(&[]);
            assert_eq!(
                actual_tools.len(),
                expected_tools.len(),
                "{adapter_name}/{case}: tools count mismatch"
            );
            for (i, exp_tool) in expected_tools.iter().enumerate() {
                let act_tool = &actual_tools[i];
                // Compare tool name
                assert_eq!(
                    act_tool.get("name"),
                    exp_tool.get("name"),
                    "{adapter_name}/{case}: tools[{i}].name mismatch"
                );
                // Compare tool description if present in expected
                if exp_tool.get("description").is_some() {
                    assert_eq!(
                        act_tool.get("description"),
                        exp_tool.get("description"),
                        "{adapter_name}/{case}: tools[{i}].description mismatch"
                    );
                }
                // Compare tool input_schema if present in expected
                if exp_tool.get("input_schema").is_some() || exp_tool.get("parameters").is_some() {
                    let act_schema = act_tool
                        .get("input_schema")
                        .or_else(|| act_tool.get("parameters"));
                    let exp_schema = exp_tool
                        .get("input_schema")
                        .or_else(|| exp_tool.get("parameters"));
                    assert_eq!(
                        act_schema, exp_schema,
                        "{adapter_name}/{case}: tools[{i}].schema mismatch"
                    );
                }
            }
        }

        // Compare sampling fields if present in output.json.
        // Adapters omit fields when they are null (serde skip_serializing_if),
        // so we treat `None` (absent) and `Some(Null)` as equivalent.
        for field in &["temperature", "top_p", "max_tokens"] {
            if let Some(expected_val) = output_json.get(*field) {
                let actual_val = actual.get(*field);
                // If expected is null, accept either absent or null in actual.
                if expected_val.is_null() {
                    assert!(
                        actual_val.is_none_or(|v| v.is_null()),
                        "{adapter_name}/{case}: {field} should be null or absent, got {actual_val:?}"
                    );
                } else if let Some(av) = actual_val {
                    assert_eq!(av, expected_val, "{adapter_name}/{case}: {field} mismatch");
                } else {
                    // expected is non-null but actual is absent
                    panic!(
                        "{adapter_name}/{case}: {field} expected {expected_val} but field is absent in encoded body"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Provider response decode tests
    // ---------------------------------------------------------------------------

    /// Test that decoding a provider response produces the expected output.json fields.
    #[expect(
        clippy::too_many_lines,
        reason = "walks one fixture's decoded response field by field (content \
                 blocks, usage, stop reason); splitting the walk would scatter \
                 the fixture's assertions across helpers"
    )]
    fn run_decode_response_fixture(adapter_name: &str, case: &str) {
        let dir = Path::new(FIXTURE_ROOT).join(adapter_name).join(case);
        let input_json = read_fixture(&dir, "input.json");
        let output_json = read_fixture(&dir, "output.json");

        let protocol = get_protocol(adapter_name);
        let target = make_target(protocol);
        let adapter = get_adapter(adapter_name);

        let bytes = serde_json::to_vec(&input_json)
            .unwrap_or_else(|e| panic!("failed to serialize input for {adapter_name}/{case}: {e}"));

        let core_resp = adapter
            .decode_response(&bytes, &target)
            .unwrap_or_else(|e| panic!("decode_response failed for {adapter_name}/{case}: {e}"));

        let actual = serde_json::to_value(&core_resp).unwrap_or_else(|e| {
            panic!("failed to serialize CoreResponse for {adapter_name}/{case}: {e}")
        });

        // Compare stop_reason (normalize to PascalCase for comparison since
        // CoreResponse serializes with default serde).
        let actual_stop = actual
            .get("stop_reason")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_stop = output_json
            .get("stop_reason")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // Normalize both to PascalCase for comparison
        let actual_stop_str = normalize_stop_reason(actual_stop.as_str().unwrap_or(""));
        let expected_stop_str = normalize_stop_reason(expected_stop.as_str().unwrap_or(""));
        assert_eq!(
            actual_stop_str, expected_stop_str,
            "{adapter_name}/{case}: stop_reason mismatch (actual: {actual_stop}, expected: {expected_stop})"
        );

        // Compare id if present in output.json
        if let Some(expected_id) = output_json.get("id") {
            assert_eq!(
                actual.get("id"),
                Some(expected_id),
                "{adapter_name}/{case}: id mismatch"
            );
        }

        // Compare model if present in output.json.
        // Adapters may override the model with the target's upstream_model, so
        // we only verify that the model field exists and has a non-empty
        // requested value. Direct string comparison is skipped because the
        // decoded model depends on ProviderAdapterTarget configuration.
        if let Some(expected_str) = output_json.get("model").and_then(|m| m.as_str())
            && let Some(am_req) = actual
                .get("model")
                .and_then(|am| am.get("requested"))
                .and_then(|v| v.as_str())
        {
            // Verify both are non-empty; the actual model may be overridden by
            // the adapter's target configuration.
            assert!(
                !expected_str.is_empty() || am_req.is_empty(),
                "{adapter_name}/{case}: expected model is empty but actual is '{am_req}'"
            );
            assert!(
                !am_req.is_empty() || expected_str.is_empty(),
                "{adapter_name}/{case}: actual model is empty but expected is '{expected_str}'"
            );
        }

        // Compare stop_sequence if present in output.json
        if let Some(expected_ss) = output_json.get("stop_sequence") {
            assert_eq!(
                actual.get("stop_sequence"),
                Some(expected_ss),
                "{adapter_name}/{case}: stop_sequence mismatch"
            );
        }

        // Compare content blocks structurally.
        // CoreContent serializes as externally-tagged enum, e.g.:
        //   {"Text": {"text": "...", "cache": null}}
        //   {"ToolUse": {"id": "...", "name": "...", "input": {...}}}
        //   {"Thinking": {"text": "...", "signature": null}}
        // The fixture output.json uses a more human-readable format:
        //   {"type": "text", "text": "..."}
        //   {"type": "tool_use", "id": "...", "name": "...", "input": {...}}
        // So we extract the actual variant from the serde key and compare.
        let actual_content = actual
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let expected_content = output_json
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            actual_content.len(),
            expected_content.len(),
            "{adapter_name}/{case}: content block count mismatch (got {}, expected {})\nactual: {actual_content:#?}\nexpected: {expected_content:#?}",
            actual_content.len(),
            expected_content.len()
        );
        for (i, (ac, ec)) in actual_content
            .iter()
            .zip(expected_content.iter())
            .enumerate()
        {
            // Actual: externally-tagged serde, extract the key (e.g. "Text", "ToolUse")
            let actual_key = extract_content_variant(ac);
            // Expected: human-readable "type" field (e.g. "text", "tool_use")
            let expected_type = ec.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
            // Normalize both to PascalCase for comparison
            let actual_normalized = match actual_key.as_str() {
                "Text" => "text",
                "ToolUse" => "tool_use",
                "ToolResult" => "tool_result",
                "Thinking" => "thinking",
                "RedactedThinking" => "redacted_thinking",
                "Image" => "image",
                "Document" => "document",
                "Audio" => "audio",
                "Video" => "video",
                "Refusal" => "refusal",
                other => other,
            };
            assert_eq!(
                actual_normalized, expected_type,
                "{adapter_name}/{case}: content[{i}] type mismatch"
            );
            // Get the inner object from the actual serde format
            let inner = ac
                .get(&actual_key)
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            // For text blocks, compare text
            if expected_type == "text" {
                assert_eq!(
                    inner.get("text"),
                    ec.get("text"),
                    "{adapter_name}/{case}: content[{i}] text mismatch"
                );
            }
            // For thinking blocks, compare text and optionally signature
            if expected_type == "thinking" {
                assert_eq!(
                    inner.get("text"),
                    ec.get("text"),
                    "{adapter_name}/{case}: content[{i}] thinking text mismatch"
                );
                // Compare signature if present in expected output
                if ec.get("signature").is_some() {
                    assert_eq!(
                        inner.get("signature"),
                        ec.get("signature"),
                        "{adapter_name}/{case}: content[{i}] thinking signature mismatch"
                    );
                }
            }
            // For tool_use blocks, compare id, name, and input
            if expected_type == "tool_use" {
                assert_eq!(
                    inner.get("name"),
                    ec.get("name"),
                    "{adapter_name}/{case}: content[{i}] tool_use name mismatch"
                );
                assert_eq!(
                    inner.get("input"),
                    ec.get("input"),
                    "{adapter_name}/{case}: content[{i}] tool_use input mismatch"
                );
                // Compare id if present in expected output
                if ec.get("id").is_some() {
                    assert_eq!(
                        inner.get("id"),
                        ec.get("id"),
                        "{adapter_name}/{case}: content[{i}] tool_use id mismatch"
                    );
                }
            }
            // For refusal blocks, compare text
            if expected_type == "refusal" {
                assert_eq!(
                    inner.get("text"),
                    ec.get("text"),
                    "{adapter_name}/{case}: content[{i}] refusal text mismatch"
                );
            }
            // For redacted_thinking blocks, compare data if present
            if expected_type == "redacted_thinking" && ec.get("data").is_some() {
                assert_eq!(
                    inner.get("data"),
                    ec.get("data"),
                    "{adapter_name}/{case}: content[{i}] redacted_thinking data mismatch"
                );
            }
        }

        // Compare usage token counts
        let actual_usage = actual
            .get("usage")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let expected_usage = output_json
            .get("usage")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        assert_eq!(
            actual_usage.get("input_tokens"),
            expected_usage.get("input_tokens"),
            "{adapter_name}/{case}: usage.input_tokens mismatch"
        );
        assert_eq!(
            actual_usage.get("output_tokens"),
            expected_usage.get("output_tokens"),
            "{adapter_name}/{case}: usage.output_tokens mismatch"
        );

        // Compare extended usage fields if present in output.json
        if expected_usage.get("reasoning_tokens").is_some() {
            assert_eq!(
                actual_usage.get("reasoning_tokens"),
                expected_usage.get("reasoning_tokens"),
                "{adapter_name}/{case}: usage.reasoning_tokens mismatch"
            );
        }
        if expected_usage.get("cache_creation_input_tokens").is_some() {
            assert_eq!(
                actual_usage.get("cache_creation_input_tokens"),
                expected_usage.get("cache_creation_input_tokens"),
                "{adapter_name}/{case}: usage.cache_creation_input_tokens mismatch"
            );
        }
        if expected_usage.get("cache_read_input_tokens").is_some() {
            assert_eq!(
                actual_usage.get("cache_read_input_tokens"),
                expected_usage.get("cache_read_input_tokens"),
                "{adapter_name}/{case}: usage.cache_read_input_tokens mismatch"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // Provider stream decode tests
    // ---------------------------------------------------------------------------

    /// Test that decoding a provider SSE stream produces events matching core-events.json.
    ///
    /// The core-events.json file uses a human-readable format with a `"type"` field
    /// per event (e.g., `{"type": "message_start", ...}`). The test compares:
    /// 1. Event counts must match.
    /// 2. Each event's variant kind (from the `"type"` field) must match.
    #[expect(
        clippy::too_many_lines,
        reason = "walks one fixture's expected SSE event sequence frame by \
                 frame; splitting the walk would scatter the fixture's \
                 assertions across helpers"
    )]
    fn run_stream_decode_fixture(adapter_name: &str, case: &str) {
        let dir = Path::new(FIXTURE_ROOT).join(adapter_name).join(case);
        let sse_input = read_fixture_raw(&dir, "input.sse");
        let core_events_raw = read_fixture_raw(&dir, "core-events.json");

        let protocol = get_protocol(adapter_name);
        let target = make_target(protocol);
        let adapter = get_adapter(adapter_name);
        let mut decoder = adapter.new_stream_decoder(&target);

        let frames = parse_sse_frames(&sse_input);
        let mut all_events: Vec<CoreEvent> = Vec::new();

        for frame in &frames {
            let events = decoder
                .decode_frame(frame)
                .unwrap_or_else(|e| panic!("decode_frame failed for {adapter_name}/{case}: {e}"));
            all_events.extend(events);
        }

        // Flush remaining state
        let remaining = decoder
            .finish()
            .unwrap_or_else(|e| panic!("finish failed for {adapter_name}/{case}: {e}"));
        all_events.extend(remaining);

        // Parse expected event types from the fixture file.
        let expected_raw: Vec<serde_json::Value> = serde_json::from_str(&core_events_raw)
            .unwrap_or_else(|e| {
                panic!("failed to parse core-events.json for {adapter_name}/{case}: {e}")
            });

        // Compare event counts.
        assert_eq!(
            all_events.len(),
            expected_raw.len(),
            "{adapter_name}/{case}: event count mismatch (got {}, expected {})",
            all_events.len(),
            expected_raw.len()
        );

        // Compare each event's variant kind with the fixture's "type" field.
        for (i, (actual, expected)) in all_events.iter().zip(expected_raw.iter()).enumerate() {
            let actual_kind = event_variant_name(actual);
            let expected_type = expected
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{adapter_name}/{case}: event[{i}] has no 'type' field"));
            assert_eq!(
                actual_kind, expected_type,
                "{adapter_name}/{case}: event[{i}] variant mismatch"
            );

            // Compare key event field values based on variant type.
            match actual {
                CoreEvent::TextDelta { index, text } => {
                    if let Some(exp_text) = expected.get("text") {
                        assert_eq!(
                            text,
                            exp_text.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] TextDelta.text mismatch"
                        );
                    }
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] TextDelta.index mismatch"
                        );
                    }
                }
                CoreEvent::ToolCallStart { index, id, name } => {
                    if let Some(exp_id) = expected.get("id") {
                        assert_eq!(
                            id,
                            exp_id.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] ToolCallStart.id mismatch"
                        );
                    }
                    if let Some(exp_name) = expected.get("name") {
                        assert_eq!(
                            name,
                            exp_name.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] ToolCallStart.name mismatch"
                        );
                    }
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] ToolCallStart.index mismatch"
                        );
                    }
                }
                CoreEvent::ToolCallDelta { index, args_delta } => {
                    if let Some(exp_args) = expected.get("args_delta") {
                        assert_eq!(
                            args_delta,
                            exp_args.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] ToolCallDelta.args_delta mismatch"
                        );
                    }
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] ToolCallDelta.index mismatch"
                        );
                    }
                }
                CoreEvent::UsageDelta { usage } => {
                    if let Some(exp_usage) = expected.get("usage") {
                        assert_eq!(
                            usage.input_tokens,
                            exp_usage
                                .get("input_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0) as i32,
                            "{adapter_name}/{case}: event[{i}] UsageDelta.usage.input_tokens mismatch"
                        );
                        assert_eq!(
                            usage.output_tokens,
                            exp_usage
                                .get("output_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0) as i32,
                            "{adapter_name}/{case}: event[{i}] UsageDelta.usage.output_tokens mismatch"
                        );
                    }
                }
                CoreEvent::MessageStop {
                    stop_reason,
                    stop_sequence,
                } => {
                    if let Some(exp_sr) = expected.get("stop_reason") {
                        let actual_sr = format!("{stop_reason:?}");
                        // Normalize: serde serializes PascalCase, fixture may use either
                        let expected_sr = normalize_stop_reason(exp_sr.as_str().unwrap_or(""));
                        let actual_normalized = normalize_stop_reason(&actual_sr);
                        assert_eq!(
                            actual_normalized, expected_sr,
                            "{adapter_name}/{case}: event[{i}] MessageStop.stop_reason mismatch"
                        );
                    }
                    if let Some(exp_ss) = expected.get("stop_sequence") {
                        let actual_ss = stop_sequence.as_deref().unwrap_or("");
                        assert_eq!(
                            actual_ss,
                            exp_ss.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] MessageStop.stop_sequence mismatch"
                        );
                    }
                }
                CoreEvent::ThinkingDelta { index, text } => {
                    if let Some(exp_text) = expected.get("text") {
                        assert_eq!(
                            text,
                            exp_text.as_str().unwrap_or(""),
                            "{adapter_name}/{case}: event[{i}] ThinkingDelta.text mismatch"
                        );
                    }
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] ThinkingDelta.index mismatch"
                        );
                    }
                }
                CoreEvent::Error { error } => {
                    if let Some(exp_msg) = expected.get("message") {
                        assert!(
                            error.message().contains(exp_msg.as_str().unwrap_or("")),
                            "{adapter_name}/{case}: event[{i}] Error.message should contain expected text"
                        );
                    }
                }
                CoreEvent::MessageStart { id, model } => {
                    if let Some(exp_id) = expected.get("id") {
                        assert_eq!(
                            id.as_deref(),
                            exp_id.as_str(),
                            "{adapter_name}/{case}: event[{i}] MessageStart.id mismatch"
                        );
                    }
                    if let Some(exp_model) = expected.get("model")
                        && let Some(exp_req) = exp_model.get("requested").and_then(|v| v.as_str())
                    {
                        assert_eq!(
                            model.requested, exp_req,
                            "{adapter_name}/{case}: event[{i}] MessageStart.model.requested mismatch"
                        );
                    }
                }
                CoreEvent::ContentStart { index, kind } => {
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] ContentStart.index mismatch"
                        );
                    }
                    if let Some(exp_kind) = expected.get("kind").and_then(|v| v.as_str()) {
                        // Use an explicit match on ContentKind to produce the expected
                        // string, rather than relying on Debug formatting which could
                        // change if the Debug impl is modified.
                        let actual_kind = match kind {
                            llm_proxy_protocol::core::ContentKind::Text => "text",
                            llm_proxy_protocol::core::ContentKind::ToolUse => "tool_use",
                            llm_proxy_protocol::core::ContentKind::Thinking => "thinking",
                            llm_proxy_protocol::core::ContentKind::Refusal => "refusal",
                            _ => "unknown",
                        };
                        assert_eq!(
                            actual_kind, exp_kind,
                            "{adapter_name}/{case}: event[{i}] ContentStart.kind mismatch"
                        );
                    }
                }
                CoreEvent::ToolCallStop { index } => {
                    if let Some(exp_index) = expected.get("index") {
                        assert_eq!(
                            *index,
                            exp_index.as_u64().unwrap_or(0) as usize,
                            "{adapter_name}/{case}: event[{i}] ToolCallStop.index mismatch"
                        );
                    }
                }
                // Ping -- variant-only comparison is sufficient.
                CoreEvent::Ping => {}
                // #[non_exhaustive] requires a wildcard arm.  New variants should
                // get explicit match arms above for field-level validation.
                _ => {}
            }
        }
    }

    /// Get the variant name of a CoreEvent for comparison.
    ///
    /// This match explicitly handles every known CoreEvent variant.  Because
    /// CoreEvent is `#[non_exhaustive]`, a wildcard arm is required, but it
    /// panics so that any new variant introduced in the protocol crate is
    /// caught immediately during test execution.
    fn event_variant_name(event: &CoreEvent) -> &'static str {
        match event {
            CoreEvent::MessageStart { .. } => "message_start",
            CoreEvent::ContentStart { .. } => "content_start",
            CoreEvent::TextDelta { .. } => "text_delta",
            CoreEvent::ThinkingDelta { .. } => "thinking_delta",
            CoreEvent::ToolCallStart { .. } => "tool_call_start",
            CoreEvent::ToolCallDelta { .. } => "tool_call_delta",
            CoreEvent::ToolCallStop { .. } => "tool_call_stop",
            CoreEvent::UsageDelta { .. } => "usage_delta",
            CoreEvent::MessageStop { .. } => "message_stop",
            CoreEvent::Error { .. } => "error",
            CoreEvent::Ping => "ping",
            // #[non_exhaustive] requires a wildcard arm.  This will panic if a
            // new CoreEvent variant is introduced without updating this match.
            _ => panic!(
                "event_variant_name: unknown CoreEvent variant. \
             Add an explicit match arm for the new variant."
            ),
        }
    }

    // ---------------------------------------------------------------------------
    // OpenAI Chat provider fixtures
    // ---------------------------------------------------------------------------

    #[test]
    fn openai_chat_encode_text_request() {
        run_encode_request_fixture(OPENAI_CHAT, "text_request");
    }

    #[test]
    fn openai_chat_decode_text_response() {
        run_decode_response_fixture(OPENAI_CHAT, "text_response");
    }

    #[test]
    fn openai_chat_encode_tool_request() {
        run_encode_request_fixture(OPENAI_CHAT, "tool_request");
    }

    #[test]
    fn openai_chat_stream_text() {
        run_stream_decode_fixture(OPENAI_CHAT, "stream_text");
    }

    #[test]
    fn openai_chat_encode_system_prompt_request() {
        run_encode_request_fixture(OPENAI_CHAT, "system_prompt_request");
    }

    #[test]
    fn openai_chat_encode_tool_result_request() {
        run_encode_request_fixture(OPENAI_CHAT, "tool_result_request");
    }

    #[test]
    fn openai_chat_encode_thinking_request() {
        run_encode_request_fixture(OPENAI_CHAT, "thinking_request");
    }

    #[test]
    fn openai_chat_decode_tool_call_response() {
        run_decode_response_fixture(OPENAI_CHAT, "tool_call_response");
    }

    #[test]
    fn openai_chat_decode_thinking_response() {
        run_decode_response_fixture(OPENAI_CHAT, "thinking_response");
    }

    #[test]
    fn openai_chat_decode_stop_reason_response() {
        run_decode_response_fixture(OPENAI_CHAT, "stop_reason_response");
    }

    #[test]
    fn openai_chat_stream_tool_call() {
        run_stream_decode_fixture(OPENAI_CHAT, "stream_tool");
    }

    #[test]
    fn openai_chat_stream_thinking() {
        run_stream_decode_fixture(OPENAI_CHAT, "stream_thinking");
    }

    #[test]
    fn openai_chat_stream_usage() {
        run_stream_decode_fixture(OPENAI_CHAT, "stream_usage");
    }

    #[test]
    fn openai_chat_stream_malformed() {
        run_stream_decode_fixture(OPENAI_CHAT, "stream_malformed");
    }

    #[test]
    fn openai_chat_decode_malformed_response() {
        let dir = Path::new(FIXTURE_ROOT)
            .join(OPENAI_CHAT)
            .join("malformed_response");
        let input_raw = read_fixture_raw(&dir, "input.json");
        let protocol = get_protocol(OPENAI_CHAT);
        let target = make_target(protocol);
        let adapter = get_adapter(OPENAI_CHAT);
        let result = adapter.decode_response(input_raw.as_bytes(), &target);
        assert!(
            result.is_err(),
            "malformed response should produce an error"
        );
    }

    // ---------------------------------------------------------------------------
    // Anthropic provider fixtures
    // ---------------------------------------------------------------------------

    #[test]
    fn anthropic_encode_text_request() {
        run_encode_request_fixture(ANTHROPIC, "text_request");
    }

    #[test]
    fn anthropic_decode_text_response() {
        run_decode_response_fixture(ANTHROPIC, "text_response");
    }

    #[test]
    fn anthropic_stream_text() {
        run_stream_decode_fixture(ANTHROPIC, "stream_text");
    }

    #[test]
    fn anthropic_encode_system_prompt_request() {
        run_encode_request_fixture(ANTHROPIC, "system_prompt_request");
    }

    #[test]
    fn anthropic_encode_tool_result_request() {
        run_encode_request_fixture(ANTHROPIC, "tool_result_request");
    }

    #[test]
    fn anthropic_encode_tool_request() {
        run_encode_request_fixture(ANTHROPIC, "tool_request");
    }

    #[test]
    fn anthropic_encode_thinking_request() {
        run_encode_request_fixture(ANTHROPIC, "thinking_request");
    }

    #[test]
    fn anthropic_decode_tool_call_response() {
        run_decode_response_fixture(ANTHROPIC, "tool_call_response");
    }

    #[test]
    fn anthropic_decode_thinking_response() {
        run_decode_response_fixture(ANTHROPIC, "thinking_response");
    }

    #[test]
    fn anthropic_decode_stop_reason_response() {
        run_decode_response_fixture(ANTHROPIC, "stop_reason_response");
    }

    #[test]
    fn anthropic_stream_tool_call() {
        run_stream_decode_fixture(ANTHROPIC, "stream_tool");
    }

    #[test]
    fn anthropic_stream_thinking() {
        run_stream_decode_fixture(ANTHROPIC, "stream_thinking");
    }

    #[test]
    fn anthropic_stream_usage() {
        run_stream_decode_fixture(ANTHROPIC, "stream_usage");
    }

    #[test]
    fn anthropic_stream_error() {
        run_stream_decode_fixture(ANTHROPIC, "stream_error");
    }

    #[test]
    fn anthropic_stream_ping() {
        run_stream_decode_fixture(ANTHROPIC, "stream_ping");
    }

    #[test]
    fn anthropic_stream_malformed() {
        run_stream_decode_fixture(ANTHROPIC, "stream_malformed");
    }

    #[test]
    fn anthropic_decode_malformed_response() {
        let dir = Path::new(FIXTURE_ROOT)
            .join(ANTHROPIC)
            .join("malformed_response");
        let input_raw = read_fixture_raw(&dir, "input.json");
        let protocol = get_protocol(ANTHROPIC);
        let target = make_target(protocol);
        let adapter = get_adapter(ANTHROPIC);
        let result = adapter.decode_response(input_raw.as_bytes(), &target);
        assert!(
            result.is_err(),
            "malformed response should produce an error"
        );
    }

    // ---------------------------------------------------------------------------
    // Responses provider fixtures
    // ---------------------------------------------------------------------------

    #[test]
    fn responses_encode_text_request() {
        run_encode_request_fixture(RESPONSES, "text_request");
    }

    #[test]
    fn responses_decode_text_response() {
        run_decode_response_fixture(RESPONSES, "text_response");
    }

    #[test]
    fn responses_encode_system_prompt_request() {
        run_encode_request_fixture(RESPONSES, "system_prompt_request");
    }

    #[test]
    fn responses_encode_tool_request() {
        run_encode_request_fixture(RESPONSES, "tool_request");
    }

    #[test]
    fn responses_encode_tool_result_request() {
        run_encode_request_fixture(RESPONSES, "tool_result_request");
    }

    #[test]
    fn responses_encode_thinking_request() {
        run_encode_request_fixture(RESPONSES, "thinking_request");
    }

    #[test]
    fn responses_decode_thinking_response() {
        run_decode_response_fixture(RESPONSES, "thinking_response");
    }

    #[test]
    fn responses_decode_tool_call_response() {
        run_decode_response_fixture(RESPONSES, "tool_call_response");
    }

    #[test]
    fn responses_decode_stop_reason_response() {
        run_decode_response_fixture(RESPONSES, "stop_reason_response");
    }

    #[test]
    fn responses_stream_text() {
        run_stream_decode_fixture(RESPONSES, "stream_text");
    }

    #[test]
    fn responses_stream_tool_call() {
        run_stream_decode_fixture(RESPONSES, "stream_tool");
    }

    #[test]
    fn responses_stream_usage() {
        run_stream_decode_fixture(RESPONSES, "stream_usage");
    }

    #[test]
    fn responses_stream_error() {
        run_stream_decode_fixture(RESPONSES, "stream_error");
    }

    #[test]
    fn responses_stream_malformed() {
        run_stream_decode_fixture(RESPONSES, "stream_malformed");
    }

    #[test]
    fn responses_decode_malformed_response() {
        let dir = Path::new(FIXTURE_ROOT)
            .join(RESPONSES)
            .join("malformed_response");
        let input_raw = read_fixture_raw(&dir, "input.json");
        let protocol = get_protocol(RESPONSES);
        let target = make_target(protocol);
        let adapter = get_adapter(RESPONSES);
        let result = adapter.decode_response(input_raw.as_bytes(), &target);
        assert!(
            result.is_err(),
            "malformed response should produce an error"
        );
    }

    // ---------------------------------------------------------------------------
    // Gemini provider fixtures
    // ---------------------------------------------------------------------------

    #[test]
    fn gemini_encode_text_request() {
        run_encode_request_fixture(GEMINI, "text_request");
    }

    #[test]
    fn gemini_decode_text_response() {
        run_decode_response_fixture(GEMINI, "text_response");
    }

    #[test]
    fn gemini_stream_text() {
        run_stream_decode_fixture(GEMINI, "stream_text");
    }

    #[test]
    fn gemini_encode_system_prompt_request() {
        run_encode_request_fixture(GEMINI, "system_prompt_request");
    }

    #[test]
    fn gemini_encode_tool_result_request() {
        run_encode_request_fixture(GEMINI, "tool_result_request");
    }

    #[test]
    fn gemini_encode_tool_request() {
        run_encode_request_fixture(GEMINI, "tool_request");
    }

    #[test]
    fn gemini_encode_thinking_request() {
        run_encode_request_fixture(GEMINI, "thinking_request");
    }

    #[test]
    fn gemini_decode_thinking_response() {
        run_decode_response_fixture(GEMINI, "thinking_response");
    }

    #[test]
    fn gemini_decode_tool_call_response() {
        run_decode_response_fixture(GEMINI, "tool_call_response");
    }

    #[test]
    fn gemini_decode_stop_reason_response() {
        run_decode_response_fixture(GEMINI, "stop_reason_response");
    }

    #[test]
    fn gemini_stream_tool_call() {
        run_stream_decode_fixture(GEMINI, "stream_tool");
    }

    #[test]
    fn gemini_stream_usage() {
        run_stream_decode_fixture(GEMINI, "stream_usage");
    }

    #[test]
    fn gemini_stream_malformed() {
        run_stream_decode_fixture(GEMINI, "stream_malformed");
    }

    #[test]
    fn gemini_decode_malformed_response() {
        let dir = Path::new(FIXTURE_ROOT)
            .join(GEMINI)
            .join("malformed_response");
        let input_raw = read_fixture_raw(&dir, "input.json");
        let protocol = get_protocol(GEMINI);
        let target = make_target(protocol);
        let adapter = get_adapter(GEMINI);
        let result = adapter.decode_response(input_raw.as_bytes(), &target);
        assert!(
            result.is_err(),
            "malformed response should produce an error"
        );
    }

    // ===========================================================================
    // Coverage-matrix completeness test
    // ===========================================================================

    /// Fixture cases required for every provider adapter (request encode direction).
    /// Format: (case_name, required_files)
    ///
    /// Universal cases apply to all adapters.  Adapter-specific extras are handled
    /// in the `coverage_matrix_all_required_fixtures_exist` match block below.
    fn required_encode_request_cases() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("text_request", vec!["core.json", "output.json"]),
            ("system_prompt_request", vec!["core.json", "output.json"]),
            ("tool_request", vec!["core.json", "output.json"]),
            ("tool_result_request", vec!["core.json", "output.json"]),
            ("thinking_request", vec!["core.json", "output.json"]),
            // Additional cases from the plan's 'Minimum fixture cases' section.
            // These are not yet enforced as hard requirements (fixtures may not
            // exist yet) but are listed here as a TODO for the next audit round:
            //   multiple_messages, tool_choice, refusal, image/document/audio/video
            //   content, cache_marker, sampling preservation, model_mapping,
            //   stop_sequence, unsupported core content, malformed client request.
        ]
    }

    /// Fixture cases required for every provider adapter (response decode direction).
    fn required_decode_response_cases() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("text_response", vec!["input.json", "output.json"]),
            ("tool_call_response", vec!["input.json", "output.json"]),
            ("thinking_response", vec!["input.json", "output.json"]),
            ("stop_reason_response", vec!["input.json", "output.json"]),
            ("malformed_response", vec!["input.json"]),
            // Additional cases planned for next audit round:
            //   redacted_thinking, refusal, usage_mapping, stop_sequence_mapping,
            //   unsupported provider fields, lossy translation warning/provider_meta.
        ]
    }

    /// Fixture cases required for every provider adapter (stream decode direction).
    fn required_stream_cases() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("stream_text", vec!["input.sse", "core-events.json"]),
            ("stream_tool", vec!["input.sse", "core-events.json"]),
            ("stream_usage", vec!["input.sse", "core-events.json"]),
            ("stream_malformed", vec!["input.sse", "core-events.json"]),
            // Additional cases planned for next audit round:
            //   stream_thinking (adapter-specific -- see match block below),
            //   stream_error (adapter-specific -- see match block below),
            //   stream_ping (adapter-specific -- see match block below),
            //   partial-tool-call-buffering, upstream-disconnect,
            //   unknown-provider-events.
        ]
    }

    /// Records every required fixture file that is absent on disk.
    fn push_missing_files(
        adapter: &str,
        cases: &[(&'static str, Vec<&'static str>)],
        missing: &mut Vec<String>,
    ) {
        for (case, files) in cases {
            let dir = Path::new(FIXTURE_ROOT).join(adapter).join(case);
            for file in files {
                if !dir.join(file).exists() {
                    missing.push(format!("{adapter}/{case}/{file}"));
                }
            }
        }
    }

    /// Records missing fixtures for the stream cases only some adapters support.
    ///
    /// Each adapter documents which extra streaming cases it supports; an
    /// adapter that cannot produce a given event (e.g. Gemini and
    /// `ThinkingDelta`) documents the exclusion here rather than omitting it
    /// silently.
    fn push_missing_adapter_specific(adapter: &str, missing: &mut Vec<String>) {
        let extra_cases: &[&str] = match adapter {
            // Anthropic supports ping, error, and thinking SSE events.
            ANTHROPIC => &["stream_error", "stream_ping", "stream_thinking"],
            // OpenAI Chat supports thinking (reasoning) streaming. It emits
            // neither Ping (no such SSE event) nor Error (upstream errors are
            // HTTP status codes, handled at the transport level).
            OPENAI_CHAT => &["stream_thinking"],
            // Responses supports error SSE events, but produces neither
            // ThinkingDelta nor Ping.
            RESPONSES => &["stream_error"],
            // Gemini's streaming tool calls are covered by the universal
            // stream_tool case; it emits no thinking, error, or ping events.
            _ => &[],
        };
        let cases: Vec<(&str, Vec<&str>)> = extra_cases
            .iter()
            .map(|case| (*case, vec!["input.sse", "core-events.json"]))
            .collect();
        push_missing_files(adapter, &cases, missing);
    }

    #[test]
    fn coverage_matrix_all_required_fixtures_exist() {
        let adapters = [OPENAI_CHAT, ANTHROPIC, RESPONSES, GEMINI];
        let mut missing: Vec<String> = Vec::new();

        for adapter in &adapters {
            for cases in [
                required_encode_request_cases(),
                required_decode_response_cases(),
                required_stream_cases(),
            ] {
                push_missing_files(adapter, &cases, &mut missing);
            }
            push_missing_adapter_specific(adapter, &mut missing);
        }

        assert!(
            missing.is_empty(),
            "missing required fixture files:\n  {}",
            missing.join("\n  ")
        );
    }

    /// Asserts every `.json` fixture present for these cases parses.
    ///
    /// `skip_malformed` exempts `malformed_response/input.json`, which is
    /// invalid on purpose.
    fn assert_json_fixtures_parse(
        adapter: &str,
        cases: &[(&'static str, Vec<&'static str>)],
        skip_malformed: bool,
    ) {
        for (case, files) in cases {
            let dir = Path::new(FIXTURE_ROOT).join(adapter).join(case);
            for file in files {
                if skip_malformed && *case == "malformed_response" && *file == "input.json" {
                    continue;
                }
                if !file.ends_with(".json") || !dir.join(file).exists() {
                    continue;
                }
                let raw = read_fixture_raw(&dir, file);
                let _: serde_json::Value = serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("invalid JSON in {adapter}/{case}/{file}: {e}"));
            }
        }
    }

    /// Asserts each stream case's `core-events.json` parses and its `input.sse`
    /// is non-empty.
    fn assert_stream_fixtures_parse(adapter: &str, cases: &[(&'static str, Vec<&'static str>)]) {
        for (case, _) in cases {
            let dir = Path::new(FIXTURE_ROOT).join(adapter).join(case);
            if dir.join("core-events.json").exists() {
                let raw = read_fixture_raw(&dir, "core-events.json");
                let _: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_else(|e| {
                    panic!("invalid core-events.json in {adapter}/{case}: {e}")
                });
            }
            if dir.join("input.sse").exists() {
                let raw = read_fixture_raw(&dir, "input.sse");
                assert!(
                    !raw.trim().is_empty(),
                    "{adapter}/{case}/input.sse should not be empty"
                );
            }
        }
    }

    #[test]
    fn coverage_matrix_all_fixture_json_is_valid() {
        let adapters = [OPENAI_CHAT, ANTHROPIC, RESPONSES, GEMINI];

        for adapter in &adapters {
            // `malformed_response/input.json` is intentionally invalid JSON.
            assert_json_fixtures_parse(adapter, &required_encode_request_cases(), false);
            assert_json_fixtures_parse(adapter, &required_decode_response_cases(), true);
            assert_stream_fixtures_parse(adapter, &required_stream_cases());
        }
    }
}
