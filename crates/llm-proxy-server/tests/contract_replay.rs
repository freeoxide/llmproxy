//! Contract replay tests (plan 02, step 1): each captured client
//! conversation fixture replays through the real router against a loopback
//! fake upstream, and the request reaching the upstream must equal the
//! fixture body apart from the edits the proxy is allowed to make
//! (model aliasing, reasoning translation, usage-on-streams). See
//! `fixtures/contract/README.md` for fixture provenance.
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use llm_proxy_adapters::ProviderAdapterRegistry;
    use llm_proxy_core::{
        AppConfig, AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRegistry, ServerConfig,
    };
    use llm_proxy_server::{AppState, BuildInfo, build_router};
    use llm_proxy_transport::ProxyClient;
    use serde_json::Value;
    use tower::ServiceExt;

    type Capture = Arc<tokio::sync::Mutex<Option<(axum::http::HeaderMap, bytes::Bytes)>>>;

    struct FakeUpstream {
        base_url: String,
        capture: Capture,
    }

    fn anthropic_success_response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_fixture_01",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "fixture reply" }],
            "model": "claude-sonnet-4-5-20250929",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 64,
                "cache_read_input_tokens": 32
            }
        }))
        .expect("serialize anthropic fixture response")
    }

    fn openai_chat_stream_body() -> Body {
        let events = vec![
            "data: {\"id\":\"chatcmpl-s\",\"object\":\"chat.completion.chunk\",\"created\":1770000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-s\",\"object\":\"chat.completion.chunk\",\"created\":1770000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-s\",\"object\":\"chat.completion.chunk\",\"created\":1770000000,\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n",
        ];
        let stream = futures::stream::iter(events.into_iter().map(|chunk| {
            Ok::<bytes::Bytes, std::convert::Infallible>(bytes::Bytes::from(chunk.to_owned()))
        }));
        Body::from_stream(stream)
    }

    fn anthropic_stream_body() -> Body {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let stream = futures::stream::iter(events.into_iter().map(|chunk| {
            Ok::<bytes::Bytes, std::convert::Infallible>(bytes::Bytes::from(chunk.to_owned()))
        }));
        Body::from_stream(stream)
    }

    fn openai_chat_success_response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "chatcmpl-fixture-01",
            "object": "chat.completion",
            "created": 1770000000,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "fixture reply" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 25, "completion_tokens": 7, "total_tokens": 32 }
        }))
        .expect("serialize chat fixture response")
    }

    async fn spawn_fake_upstream(anthropic: bool, stream: bool) -> FakeUpstream {
        spawn_fake_upstream_dialect(anthropic, stream, false).await
    }

    async fn spawn_fake_upstream_dialect(
        anthropic: bool,
        stream: bool,
        chat_stream: bool,
    ) -> FakeUpstream {
        let capture: Capture = Arc::new(tokio::sync::Mutex::new(None));
        let capture_for_handler = Arc::clone(&capture);
        let is_stream = stream;

        let handler = Arc::new(move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
            let capture = Arc::clone(&capture_for_handler);
            async move {
                *capture.lock().await = Some((headers, body));
                if is_stream {
                    let stream_body = match chat_stream {
                        true => openai_chat_stream_body(),
                        false => anthropic_stream_body(),
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        stream_body,
                    )
                        .into_response()
                } else if anthropic {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        anthropic_success_response(),
                    )
                        .into_response()
                } else {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        openai_chat_success_response(),
                    )
                        .into_response()
                }
            }
        });

        let app = Router::new()
            .route(
                "/v1/messages",
                post({
                    let handler = Arc::clone(&handler);
                    move |headers, body| handler(headers, body)
                }),
            )
            .route(
                "/v1/chat/completions",
                post({
                    let handler = Arc::clone(&handler);
                    move |headers, body| handler(headers, body)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fake upstream serves");
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fake upstream did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        FakeUpstream {
            base_url: format!("http://{addr}"),
            capture,
        }
    }

    fn state_with_provider(
        upstream: &FakeUpstream,
        protocol: &str,
        route: &str,
        lossy: bool,
    ) -> AppState {
        let adapter_name = "upstream";
        let provider = ProviderConfig {
            name: "fixture-provider".to_owned(),
            api_key: secrecy::SecretString::from("operator-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                adapter_name.to_owned(),
                ProviderAdapterConfig {
                    protocol: protocol.to_owned(),
                    endpoint: match protocol {
                        "anthropic_messages" => format!("{}/v1/messages", upstream.base_url),
                        _ => format!("{}/v1/chat/completions", upstream.base_url),
                    },
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy,
                },
            )]),
            routes: llm_proxy_core::ProviderRoutesConfig {
                messages: (route == "messages").then_some(adapter_name.to_owned()),
                chat_completions: (route == "chat_completions").then_some(adapter_name.to_owned()),
            },
            model_aliases: HashMap::from([(
                "claude-sonnet-4-5".to_owned(),
                "claude-sonnet-4-5-20250929".to_owned(),
            )]),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        AppState::new(
            AppConfig {
                server: ServerConfig {
                    bind: "127.0.0.1:3456".parse().expect("bind"),
                    request_timeout: Duration::from_secs(300),
                    shutdown_timeout: Duration::from_secs(30),
                    log_level: "info".to_owned(),
                    hot_reload: false,
                    allowed_origins: None,
                    server_name: "contract-test".to_owned(),
                    rate_limit_rpm: 1000,
                    trust_forwarded_headers: false,
                    dedup_window: Duration::ZERO,
                    log_format: Default::default(),
                    admin_bind: None,
                    admin_on_main_listener: false,
                    endpoint_families: Default::default(),
                    upstream_timeouts: Default::default(),
                    bare_v1: Default::default(),
                },
                auth: Default::default(),
                storage: None,
            },
            registry,
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            BuildInfo {
                name: "test",
                version: "0.0.0",
                target: "test",
                git_sha: "test",
            },
        )
    }

    fn fixture(path: &str) -> Value {
        let text = std::fs::read_to_string(format!(
            "{}/tests/fixtures/contract/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("read fixture {path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse fixture {path}: {e}"))
    }

    /// A fixture captured live from a real client (see
    /// `fixtures/contract/captured/` and its README rows).
    fn captured_fixture(path: &str) -> Value {
        fixture(&format!("captured/{path}"))
    }

    /// The header set a real client sent alongside a captured body
    /// (transport headers removed, credential values already fake
    /// placeholders), applied verbatim to a replayed request.
    fn apply_captured_headers(
        builder: axum::http::request::Builder,
        path: &str,
    ) -> axum::http::request::Builder {
        let headers = fixture(&format!("captured/{path}"));
        let map = headers
            .as_object()
            .unwrap_or_else(|| panic!("captured headers {path} must be an object"));
        let mut builder = builder;
        for (name, value) in map {
            let value = value
                .as_str()
                .unwrap_or_else(|| panic!("captured header {name} must be a string"));
            let name = axum::http::HeaderName::try_from(name.as_str())
                .unwrap_or_else(|e| panic!("captured header name {name:?} must be valid: {e}"));
            builder = builder.header(name, value);
        }
        builder
    }

    /// Returns the (headers, body) the fake upstream received.
    async fn captured(upstream: &FakeUpstream) -> (axum::http::HeaderMap, Value) {
        let guard = upstream.capture.lock().await;
        let (headers, body) = guard.as_ref().expect("upstream received a request");
        let json: Value = serde_json::from_slice(body).expect("upstream body is JSON");
        (headers.clone(), json)
    }

    /// Headers a real Claude Code / SDK client sends; the secrets are fake.
    fn apply_client_headers(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header("content-type", "application/json")
            .header("authorization", "Bearer client-secret-token")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "context-1m-2025-08-07")
            .header("x-claude-code-session-id", "sess-fixture-1")
            .header("origin", "https://evil.example")
            .header("user-agent", "claude-cli/2.0.0 (external, cli)")
            .header("accept-language", "en-US,en;q=0.9")
            .header("x-request-id", "client-generated-id")
    }

    #[tokio::test]
    async fn claude_code_session_replays_byte_identical_modulo_model() {
        let upstream = spawn_fake_upstream(true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "messages", false);
        let app = build_router(state);

        let body = fixture("claude-code/session-request.json");
        let builder = apply_client_headers(
            Request::builder()
                .method("POST")
                .uri("/providers/fixture-provider/v1/messages"),
        );
        let resp = app
            .oneshot(builder.body(Body::from(body.to_string())).expect("body"))
            .await
            .expect("request served");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "same-format replay must pass"
        );

        let (headers, upstream_body) = captured(&upstream).await;
        // The upstream body must equal the fixture except the model alias and the
        // clamped thinking budget (the fixture budget of 10000 is already valid).
        let mut expected = body.clone();
        expected["model"] = Value::String("claude-sonnet-4-5-20250929".to_owned());
        assert_eq!(
            upstream_body, expected,
            "the request reaching upstream must be the client body modulo the listed edits"
        );
        // Every previously-dropped Anthropic field survives.
        assert_eq!(upstream_body["stop_sequences"][0], "\n\nHuman:");
        assert_eq!(upstream_body["top_k"], 0);
        assert_eq!(
            upstream_body["tool_choice"]["disable_parallel_tool_use"],
            false
        );
        assert_eq!(upstream_body["metadata"]["user_id"], "user_abc123");
        // Beta feature through the proxy (done-when #3) plus the session header.
        assert_eq!(
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("context-1m-2025-08-07")
        );
        assert_eq!(
            headers
                .get("x-claude-code-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("sess-fixture-1")
        );
        // Client auth, Origin and browser headers never reach upstream. The
        // upstream `authorization` is the operator's configured key; the client's
        // token value must not appear anywhere.
        for forbidden in ["origin", "user-agent", "accept-language", "x-request-id"] {
            assert!(
                !headers.contains_key(forbidden),
                "{forbidden} must never be forwarded, saw {forbidden}"
            );
        }
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer operator-key"),
            "only the operator credential may authenticate upstream"
        );
        for (_, value) in headers.iter() {
            let text = value.to_str().unwrap_or_default();
            assert!(
                !text.contains("client-secret-token"),
                "the client credential must never leak upstream: {text}"
            );
        }
        // Client-supplied request ids are never adopted: the response echoes the
        // proxy-generated id, so logs and the ledger stay keyed on our own ids.
        let response_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            !response_id.is_empty() && response_id != "client-generated-id",
            "the proxy must answer with its own request id, got {response_id:?}"
        );
    }

    #[tokio::test]
    async fn anthropic_sdk_tool_replay_reaches_upstream_unchanged() {
        let upstream = spawn_fake_upstream(true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "messages", false);
        let app = build_router(state);

        let body = fixture("anthropic-sdk/tool-replay-request.json");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/providers/fixture-provider/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, upstream_body) = captured(&upstream).await;
        let mut expected = body.clone();
        expected["model"] = Value::String("claude-sonnet-4-5-20250929".to_owned());
        assert_eq!(upstream_body, expected);
        // Tool-state replay: the error tool_result and the image block survive.
        assert_eq!(upstream_body["messages"][2]["content"][0]["is_error"], true);
        assert_eq!(
            upstream_body["messages"][0]["content"][0]["source"]["media_type"],
            "image/png"
        );
    }

    #[tokio::test]
    async fn openai_sdk_fields_all_reach_the_upstream() {
        let upstream = spawn_fake_upstream(false, false).await;
        let state = state_with_provider(
            &upstream,
            "openai_chat_completions",
            "chat_completions",
            false,
        );
        let app = build_router(state);

        let body = fixture("openai-sdk/chat-request.json");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/providers/fixture-provider/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer client-secret")
                    .body(Body::from(body.to_string()))
                    .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, upstream_body) = captured(&upstream).await;
        // Same-format passthrough: byte-identical apart from reasoning
        // translation (the fixture's explicit reasoning_effort survives as-is).
        let mut expected = body.clone();
        // No model alias configured for gpt-4o on this provider, so the model
        // field is unchanged; thinking is absent in the fixture.
        let _ = &mut expected;
        assert_eq!(upstream_body, expected);
        // The eleven fields the audit found dropped (row 8) all survive.
        for field in [
            "response_format",
            "parallel_tool_calls",
            "seed",
            "n",
            "logprobs",
            "logit_bias",
            "frequency_penalty",
            "presence_penalty",
            "service_tier",
            "prompt_cache_key",
            "store",
        ] {
            assert!(
                upstream_body.get(field).is_some(),
                "{field} must reach the upstream (audit row 8): {upstream_body}"
            );
        }
        // Multimodal image parts survive byte-for-byte.
        assert_eq!(
            upstream_body["messages"][1]["content"][1]["image_url"]["url"],
            body["messages"][1]["content"][1]["image_url"]["url"]
        );
    }

    #[tokio::test]
    async fn openai_sdk_stream_request_gets_usage_injection_only() {
        let upstream = spawn_fake_upstream_dialect(false, true, true).await;
        let state = state_with_provider(
            &upstream,
            "openai_chat_completions",
            "chat_completions",
            false,
        );
        let app = build_router(state);

        let body = fixture("openai-sdk/stream-request.json");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/providers/fixture-provider/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, upstream_body) = captured(&upstream).await;
        // The one allowed edit on chat streams: include_usage is forced true so
        // the ledger records real usage.
        assert_eq!(
            upstream_body["stream_options"]["include_usage"], true,
            "the proxy must request usage on streams: {upstream_body}"
        );
        assert_eq!(upstream_body["stream"], true);
        assert_eq!(upstream_body["temperature"], 0.1);
    }

    #[tokio::test]
    async fn cross_format_rejection_names_the_fields_and_lossy_drops_them() {
        // OpenAI-SDK chat fixture against an Anthropic Messages upstream: fields
        // with no Anthropic equivalent must fail the request naming them, and the
        // per-route lossy opt-in must turn that into a drop with the mapped and
        // carried fields intact.
        let body = fixture("openai-sdk/chat-request.json");

        let upstream = spawn_fake_upstream(true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "chat_completions", false);
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/providers/fixture-provider/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .expect("body"),
        )
        .expect("error json");
        let full = resp_json.to_string();
        for rejected in ["response_format", "seed", "logprobs"] {
            assert!(
                full.contains(rejected),
                "the rejection must name {rejected}: {resp_json}"
            );
        }

        let upstream = spawn_fake_upstream(true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "chat_completions", true);
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/providers/fixture-provider/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);
        let (_, upstream_body) = captured(&upstream).await;
        // Carried verbatim: Anthropic accepts service_tier.
        assert_eq!(upstream_body["service_tier"], "flex");
        // Mapped: parallel_tool_calls -> tool_choice.disable_parallel_tool_use.
        assert_eq!(
            upstream_body["tool_choice"]["disable_parallel_tool_use"],
            true
        );
        // Mapped: reasoning_effort is translated at the Anthropic edge (BUG-20)
        // — never forwarded as an OpenAI field. With this fixture's
        // max_completion_tokens of 1024 the derived 8192-token budget cannot fit
        // below max_tokens, so thinking is dropped rather than sent invalid.
        assert!(
            upstream_body.get("reasoning_effort").is_none(),
            "reasoning_effort must never reach the Anthropic wire: {upstream_body}"
        );
        assert!(
            upstream_body.get("max_completion_tokens").is_none(),
            "the Anthropic edge must use max_tokens: {upstream_body}"
        );
        assert_eq!(upstream_body["max_tokens"], 1024);
        // Dropped under lossy: knobs with no Anthropic equivalent.
        for dropped in [
            "response_format",
            "seed",
            "logprobs",
            "logit_bias",
            "store",
            "prompt_cache_key",
        ] {
            assert!(
                upstream_body.get(dropped).is_none(),
                "{dropped} must be dropped on a lossy route: {upstream_body}"
            );
        }
        // The multimodal part maps: the OpenAI data-URI image becomes an
        // Anthropic base64 image block (plan step 4: carry image_url).
        assert_eq!(
            upstream_body["messages"][0]["content"][1]["type"], "image",
            "the image part must map to an Anthropic image block: {upstream_body}"
        );
        assert_eq!(
            upstream_body["messages"][0]["content"][1]["source"],
            serde_json::json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg=="})
        );
    }

    #[tokio::test]
    async fn gemini_upstream_preserves_thought_signatures_across_turns() {
        use llm_proxy_protocol::core::{
            CoreContent, CoreMessage, CoreRequest, CoreRole, ModelRef, ProviderHints,
            RequestMetadata, SamplingOptions,
        };

        // An Anthropic-dialect client replays a tool_use block carrying a
        // thought signature the proxy previously returned for a Gemini upstream.
        let adapter = ProviderAdapterRegistry::builtin();
        let gemini = adapter
            .get(llm_proxy_adapters::protocols::GEMINI_GENERATE_CONTENT)
            .expect("gemini adapter");
        let mut target = llm_proxy_adapters::ProviderAdapterTarget {
            provider_name: "fixture-provider".into(),
            adapter_name: "upstream".into(),
            protocol: llm_proxy_adapters::ProtocolName::new(
                llm_proxy_adapters::protocols::GEMINI_GENERATE_CONTENT,
            ),
            // The test asserts only the encoded body; no request is sent.
            endpoint: "https://gemini.example/v1beta/models/{model}:generateContent".into(),
            auth_style: AuthStyle::Bearer,
            api_key: secrecy::SecretString::from("operator-key"),
            requested_model: "gemini-2.5-pro".into(),
            upstream_model: "gemini-2.5-pro".into(),
            headers: Arc::new(HashMap::new()),
            lossy: false,
            forward_inbound_headers: std::sync::Arc::new(Vec::new()),
        };
        target.upstream_model = "gemini-2.5-pro".into();

        let core = CoreRequest {
            model: ModelRef {
                requested: "gemini-2.5-pro".into(),
                upstream: None,
            },
            system: vec![],
            messages: vec![
                CoreMessage {
                    role: CoreRole::Assistant,
                    content: vec![CoreContent::ToolUse {
                        id: "gemini_call_0".into(),
                        name: "get_weather".into(),
                        input: serde_json::json!({"city": "SF"}),
                        signature: Some("c2lnLWZpeHR1cmU=".into()),
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
                        cache: None,
                    }],
                },
            ],
            tools: vec![],
            tool_choice: None,
            sampling: SamplingOptions::default(),
            stream: false,
            metadata: RequestMetadata::default(),
            provider_hints: ProviderHints::default(),
        };
        let wire = llm_proxy_adapters::ClientWireRequest::new(
            llm_proxy_adapters::ClientWireFormat::AnthropicMessages,
            serde_json::json!({"model": "gemini-2.5-pro", "messages": []}),
        );
        let proxy_req = gemini
            .encode_request(&core, &target, &wire)
            .expect("gemini encode");
        let body: Value = serde_json::from_slice(&proxy_req.body).expect("gemini body");

        // The replayed functionCall part carries the thought signature, and the
        // functionResponse part echoes it back (backlog 16).
        let parts = body["contents"][0]["parts"]
            .as_array()
            .expect("assistant parts");
        assert_eq!(
            parts[0]["thoughtSignature"], "c2lnLWZpeHR1cmU=",
            "the replayed functionCall part must carry the thought signature: {body}"
        );
        let response_parts = body["contents"][1]["parts"].as_array().expect("tool parts");
        assert_eq!(
            response_parts[0]["thoughtSignature"], "c2lnLWZpeHR1cmU=",
            "the functionResponse part must echo the thought signature: {body}"
        );
    }

    #[tokio::test]
    async fn captured_claude_code_session_replays_with_only_the_model_edit() {
        // Live-captured session (Claude Code CLI 2.1.251, 2026-11-05): a
        // streaming request whose thinking object carries the auxiliary
        // `display` subfield and whose top level carries `context_management`.
        // Both must ride the same-format passthrough untouched; the only
        // expected edit is the model alias (plus the reasoning knobs being
        // re-derived, which for this valid budget is value-identical).
        let upstream = spawn_fake_upstream(true, true).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "messages", false);
        let app = build_router(state);

        let body = captured_fixture("claude-code/session-request.json");
        let builder = apply_captured_headers(
            Request::builder()
                .method("POST")
                .uri("/providers/fixture-provider/v1/messages"),
            "claude-code/headers.json",
        );
        let resp = app
            .oneshot(builder.body(Body::from(body.to_string())).expect("body"))
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (headers, upstream_body) = captured(&upstream).await;
        let mut expected = body.clone();
        expected["model"] = Value::String("claude-sonnet-4-5-20250929".to_owned());
        assert_eq!(
            upstream_body, expected,
            "the captured session must reach upstream with only the model rewritten"
        );
        // The live capture found these two fields; they must survive.
        assert_eq!(upstream_body["thinking"]["display"], "omitted");
        assert_eq!(
            upstream_body["context_management"]["edits"][0]["type"],
            "clear_thinking_20251015"
        );
        // The full five-beta list the real client sent is forwarded verbatim.
        assert_eq!(
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some(
                "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
            )
        );
        assert_eq!(
            headers
                .get("x-claude-code-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("9205b62f-b709-4c0a-90b4-69402652f634")
        );
        // The real client authenticates with x-api-key (not authorization) and
        // sends SDK telemetry headers; none of that may reach the upstream.
        for forbidden in [
            "x-api-key",
            "x-app",
            "x-stainless-lang",
            "x-stainless-package-version",
            "anthropic-dangerous-direct-browser-access",
            "user-agent",
        ] {
            assert!(
                !headers.contains_key(forbidden),
                "{forbidden} from the captured client set must never be forwarded"
            );
        }
        for (_, value) in headers.iter() {
            let text = value.to_str().unwrap_or_default();
            assert!(
                !text.contains("sk-ant-api03-capture-fixture-key-not-real"),
                "the captured client credential must never leak upstream: {text}"
            );
        }
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer operator-key")
        );
    }

    #[tokio::test]
    async fn captured_anthropic_sdk_tool_replay_reaches_upstream_modulo_model() {
        // Live-captured from @anthropic-ai/sdk 1.5.0 (JS) on 2026-11-05.
        let upstream = spawn_fake_upstream(true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "messages", false);
        let app = build_router(state);

        let body = captured_fixture("anthropic-sdk/tool-replay-request.json");
        let builder = apply_captured_headers(
            Request::builder()
                .method("POST")
                .uri("/providers/fixture-provider/v1/messages"),
            "anthropic-sdk/headers.json",
        );
        let resp = app
            .oneshot(builder.body(Body::from(body.to_string())).expect("body"))
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (headers, upstream_body) = captured(&upstream).await;
        let mut expected = body.clone();
        expected["model"] = Value::String("claude-sonnet-4-5-20250929".to_owned());
        assert_eq!(
            upstream_body, expected,
            "the captured SDK conversation must reach upstream with only the model rewritten"
        );
        // The fields the audit previously lost all survive the real SDK wire.
        assert_eq!(upstream_body["top_k"], 40);
        assert_eq!(upstream_body["stop_sequences"][0], "\n\nHuman:");
        assert_eq!(upstream_body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            upstream_body["tool_choice"]["disable_parallel_tool_use"],
            true
        );
        assert_eq!(
            upstream_body["messages"][2]["content"][0]["is_error"],
            false
        );
        assert_eq!(
            upstream_body["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        // SDK telemetry never reaches the upstream; the adapter pins its own
        // anthropic-version rather than forwarding the client's.
        for forbidden in [
            "x-stainless-lang",
            "x-stainless-os",
            "x-stainless-runtime",
            "sec-fetch-mode",
        ] {
            assert!(
                !headers.contains_key(forbidden),
                "{forbidden} from the captured SDK set must never be forwarded"
            );
        }
        assert!(headers.contains_key("anthropic-version"));
        for (_, value) in headers.iter() {
            let text = value.to_str().unwrap_or_default();
            assert!(
                !text.contains("sk-ant-api03-capture-fixture-key-not-real"),
                "the captured SDK credential must never leak upstream: {text}"
            );
        }
    }

    #[tokio::test]
    async fn captured_anthropic_sdk_stream_request_replays_unedited() {
        // The SDK's streaming shape: no usage-injection edit exists on the
        // Anthropic wire, so the captured body must pass completely unedited
        // apart from the model alias.
        let upstream = spawn_fake_upstream_dialect(true, true, false).await;
        let state = state_with_provider(&upstream, "anthropic_messages", "messages", false);
        let app = build_router(state);

        let body = captured_fixture("anthropic-sdk/stream-request.json");
        let resp = app
            .oneshot(
                apply_captured_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/providers/fixture-provider/v1/messages"),
                    "anthropic-sdk/headers.json",
                )
                .body(Body::from(body.to_string()))
                .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, upstream_body) = captured(&upstream).await;
        let mut expected = body.clone();
        expected["model"] = Value::String("claude-sonnet-4-5-20250929".to_owned());
        assert_eq!(upstream_body, expected);
        assert_eq!(upstream_body["stream"], true);
    }

    #[tokio::test]
    async fn captured_openai_sdk_chat_replays_verbatim() {
        // Live-captured from the openai JS SDK 7.51.0 on 2026-11-05: the full
        // knob set including the eleven fields audit row 8 found dropped.
        let upstream = spawn_fake_upstream(false, false).await;
        let state = state_with_provider(
            &upstream,
            "openai_chat_completions",
            "chat_completions",
            false,
        );
        let app = build_router(state);

        let body = captured_fixture("openai-sdk/chat-request.json");
        let resp = app
            .oneshot(
                apply_captured_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/providers/fixture-provider/v1/chat/completions"),
                    "openai-sdk/headers.json",
                )
                .body(Body::from(body.to_string()))
                .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (headers, upstream_body) = captured(&upstream).await;
        assert_eq!(
            upstream_body, body,
            "the captured chat request must reach upstream byte-identical"
        );
        for field in [
            "response_format",
            "parallel_tool_calls",
            "seed",
            "logit_bias",
            "frequency_penalty",
            "presence_penalty",
            "service_tier",
            "store",
            "stop",
            "user",
            "max_completion_tokens",
        ] {
            assert!(
                upstream_body.get(field).is_some(),
                "{field} from the live SDK capture must reach the upstream: {upstream_body}"
            );
        }
        // The replayed tool_calls stay string-encoded and the image part
        // keeps its detail level, exactly as the SDK sent them.
        assert_eq!(
            upstream_body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"SF\"}"
        );
        assert_eq!(
            upstream_body["messages"][1]["content"][1]["image_url"]["detail"],
            "low"
        );
        // SDK telemetry and the browser-mode header never reach the upstream.
        for forbidden in ["x-stainless-lang", "sec-fetch-mode", "user-agent"] {
            assert!(
                !headers.contains_key(forbidden),
                "{forbidden} from the captured SDK set must never be forwarded"
            );
        }
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer operator-key")
        );
    }

    #[tokio::test]
    async fn captured_openai_sdk_stream_request_keeps_usage_injection_only() {
        // The SDK already asked for usage; the proxy must keep it on and make
        // no other edit to the captured streaming body.
        let upstream = spawn_fake_upstream_dialect(false, true, true).await;
        let state = state_with_provider(
            &upstream,
            "openai_chat_completions",
            "chat_completions",
            false,
        );
        let app = build_router(state);

        let body = captured_fixture("openai-sdk/stream-request.json");
        let resp = app
            .oneshot(
                apply_captured_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/providers/fixture-provider/v1/chat/completions"),
                    "openai-sdk/headers.json",
                )
                .body(Body::from(body.to_string()))
                .expect("body"),
            )
            .await
            .expect("request served");
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, upstream_body) = captured(&upstream).await;
        assert_eq!(
            upstream_body, body,
            "the captured stream request must reach upstream unedited (usage already on)"
        );
        assert_eq!(upstream_body["stream_options"]["include_usage"], true);
        assert_eq!(upstream_body["temperature"], 0.1);
    }
}
