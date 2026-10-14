//! Smoke test: the core module is publicly exported and its primary types
//! construct and round-trip through serde.

#[cfg(test)]
mod tests {

    use llm_proxy_protocol::core::{
        CacheControl, CacheControlType, ContentKind, CoreContent, CoreEvent, CoreMessage,
        CoreRequest, CoreResponse, CoreRole, CoreStreamError, CoreStreamErrorKind, CoreTool,
        CoreToolChoice, ModelRef, ProviderHints, RequestMetadata, SamplingOptions, StopReason,
        Usage, UsageProvenance,
    };

    #[test]
    fn core_types_are_exported() {
        let _request: CoreRequest;
        let _response: CoreResponse;
        let _event: CoreEvent;
        let _content_kind: ContentKind;
        let _stream_error: CoreStreamError;
        let _stream_error_kind: CoreStreamErrorKind;
        let _tool: CoreTool;
        let _tool_choice: CoreToolChoice;
        let _stop_reason: StopReason;
        let _usage: Usage;
        let _provenance: UsageProvenance;
    }

    #[test]
    fn core_request_serializes() {
        let req = CoreRequest {
            model: ModelRef {
                requested: "test-model".into(),
                upstream: None,
            },
            system: vec![CoreContent::Text {
                text: "be helpful".into(),
                cache: None,
            }],
            messages: vec![CoreMessage {
                role: CoreRole::User,
                content: vec![CoreContent::Text {
                    text: "hello".into(),
                    cache: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            sampling: SamplingOptions::default(),
            stream: false,
            metadata: RequestMetadata::default(),
            provider_hints: ProviderHints::default(),
        };
        let json = serde_json::to_string(&req).expect("CoreRequest should serialize");
        assert!(!json.is_empty());
        let back: CoreRequest =
            serde_json::from_str(&json).expect("CoreRequest should deserialize");
        assert_eq!(back.model.requested, "test-model");
    }

    #[test]
    fn cache_control_type_is_exported() {
        let cc = CacheControl {
            r#type: CacheControlType::Ephemeral,
        };
        assert_eq!(
            serde_json::to_string(&cc).unwrap(),
            r#"{"type":"ephemeral"}"#
        );
    }

    #[test]
    fn core_event_round_trips() {
        let events = vec![
            CoreEvent::Ping,
            CoreEvent::TextDelta {
                index: 0,
                text: "hello".into(),
            },
            CoreEvent::MessageStart {
                id: Some("msg_123".into()),
                model: ModelRef {
                    requested: "test-model".into(),
                    upstream: None,
                },
            },
        ];
        for event in &events {
            let json = serde_json::to_string(event).expect("CoreEvent should serialize");
            assert!(!json.is_empty());
            let back: CoreEvent =
                serde_json::from_str(&json).expect("CoreEvent should deserialize");
            assert_eq!(&back, event, "CoreEvent round-trip should be lossless");
        }
    }

    #[test]
    fn core_request_full_round_trip() {
        let req = CoreRequest {
            model: ModelRef {
                requested: "test-model".into(),
                upstream: None,
            },
            system: vec![CoreContent::Text {
                text: "be helpful".into(),
                cache: None,
            }],
            messages: vec![CoreMessage {
                role: CoreRole::User,
                content: vec![CoreContent::Text {
                    text: "hello".into(),
                    cache: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            sampling: SamplingOptions::default(),
            stream: false,
            metadata: RequestMetadata::default(),
            provider_hints: ProviderHints::default(),
        };
        let json = serde_json::to_string(&req).expect("CoreRequest should serialize");
        let back: CoreRequest =
            serde_json::from_str(&json).expect("CoreRequest should deserialize");
        assert_eq!(
            req, back,
            "CoreRequest round-trip should be structurally equal"
        );
    }

    #[test]
    fn core_response_full_round_trip() {
        let mut provider_meta = serde_json::Map::new();
        provider_meta.insert("log_id".into(), serde_json::Value::String("log_abc".into()));
        let resp = CoreResponse {
            id: Some("resp_round_trip".into()),
            model: ModelRef {
                requested: "test-model".into(),
                upstream: None,
            },
            content: vec![
                CoreContent::Thinking {
                    text: "reasoning about the question".into(),
                    signature: Some("sig_abc".into()),
                },
                CoreContent::Text {
                    text: "Here is my answer".into(),
                    cache: None,
                },
                CoreContent::ToolUse {
                    id: "call_1".into(),
                    name: "search".into(),
                    input: serde_json::json!({"query": "rust serde"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            stop_sequence: Some("\n".into()),
            usage: Usage {
                input_tokens: 42,
                output_tokens: 87,
                reasoning_tokens: Some(15),
                cache_creation_input_tokens: Some(100),
                cache_read_input_tokens: None,
                provenance: UsageProvenance::ProviderReported,
            },
            provider_meta,
            cost: None,
        };
        let json = serde_json::to_string(&resp).expect("CoreResponse should serialize");
        let back: CoreResponse =
            serde_json::from_str(&json).expect("CoreResponse should deserialize");
        assert_eq!(
            resp, back,
            "CoreResponse round-trip should be structurally equal"
        );
    }
}
