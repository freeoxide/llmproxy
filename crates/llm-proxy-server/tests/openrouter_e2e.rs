//! End-to-end verification of the proxy's *goals* against faithful OpenRouter
//! response shapes.
//!
//! OpenRouter (<https://openrouter.ai>) speaks the OpenAI Chat Completions
//! wire protocol, but its real responses carry OpenRouter-specific extras that a
//! vanilla OpenAI fixture does not: `id`s prefixed `gen-…`, a top-level
//! `provider` routing field, a `cost` member inside `usage`, `system_fingerprint`,
//! `native_finish_reason`, `logprobs`, and its own `{"error":{"code","message",
//! "metadata"}}` envelope on 402/429. This suite drives the *real* in-process
//! proxy pipeline (`build_router` + `AppState`) against a mock upstream that
//! returns those exact shapes, so the assertions validate behaviour against
//! reality rather than hand-tidied fixtures.
//!
//! Each test is mapped to a documented goal of the proxy:
//!
//! | Goal                                  | Test                                                  |
//! |---------------------------------------|-------------------------------------------------------|
//! | Protocol hub: OpenAI client passthrough | `openai_client_*_translates_to_openai_shape`        |
//! | Cross-protocol translation (headline) | `anthropic_client_*_translates_to_anthropic_shape`   |
//! | Tool-call fidelity (OpenAI client)    | `tool_calls_preserved_for_openai_client`             |
//! | Tool-call fidelity (cross-protocol)   | `tool_calls_translate_to_anthropic_tool_use_blocks`  |
//! | Robustness to provider-specific extras | `openrouter_specific_extras_do_not_break_conversion`|
//! | Streaming fidelity (OpenAI client)    | `openai_client_stream_ends_with_done`                |
//! | Streaming fidelity (cross-protocol)   | `anthropic_client_stream_cross_protocol`             |
//! | OpenRouter header injection           | `openrouter_app_headers_forwarded_upstream`          |
//! | Error fidelity (429 passthrough)      | `openrouter_429_surfaces_as_client_shaped_error`     |
//! | Live network validation (opt-in)      | `live_openrouter_round_trip` (`#[ignore]`)           |
//!
//! The first nine run offline and deterministically in CI. The last is
//! `#[ignore]`-gated on `OPENROUTER_API_KEY` so the real path can be re-validated
//! on demand without key material or network in CI.

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
        response::sse::{Event, KeepAlive, Sse},
        routing::post,
    };
    use futures::FutureExt;
    use llm_proxy_core::{
        AppConfig, AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRegistry, ServerConfig,
    };
    use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
    use llm_proxy_server::{AppState, BuildInfo, build_router};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    // ===========================================================================
    // Mock-server plumbing (mirrors the established harness in core_pipeline.rs)
    // ===========================================================================

    /// Poll the mock server's TCP port until it accepts a connection or 5 s elapse.
    async fn wait_for_ready(addr: std::net::SocketAddr) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("mock server at {addr} did not become ready within 5 s");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Spawn a mock axum server, surfacing a serve-task panic loudly (see the audit
    /// note on `mock-server-joinhandle-swallow` in core_pipeline.rs).
    fn spawn_mock_serve(listener: tokio::net::TcpListener, app: Router) {
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(axum::serve(listener, app).into_future())
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("mock axum::serve error: {e}"),
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| panic.downcast_ref::<&'static str>().copied())
                        .unwrap_or("<non-string panic>");
                    eprintln!("mock server task panicked: {msg}");
                    std::process::abort();
                }
            }
        });
    }

    /// A canned non-streaming OpenRouter Chat Completions response.
    ///
    /// Deliberately rich in OpenRouter-specific extras (`gen-` id, top-level
    /// `provider`, `cost` inside `usage`, `system_fingerprint`,
    /// `native_finish_reason`, `logprobs`, `refusal`) so that the "robustness to
    /// provider variance" goal is exercised by every test that consumes it.
    fn openrouter_non_stream_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "gen-1719000000-AbCdEf",
            "provider": "OpenAI",
            "model": "openai/gpt-4o",
            "object": "chat.completion",
            "created": 1719000000,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from OpenRouter!",
                    "refusal": null
                },
                "finish_reason": "stop",
                "native_finish_reason": "stop",
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20,
                "cost": 0.000123
            },
            "system_fingerprint": "fp_4e7e1a2b3c"
        }))
        .unwrap()
    }

    /// A canned non-streaming OpenRouter tool-call response.
    fn openrouter_tool_call_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "gen-toolcall-001",
            "provider": "OpenAI",
            "model": "openai/gpt-4o",
            "object": "chat.completion",
            "created": 1719000000,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_or_abc123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"San Francisco\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls",
                "native_finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 42,
                "completion_tokens": 18,
                "total_tokens": 60
            }
        }))
        .unwrap()
    }

    /// A canned OpenRouter rate-limit (429) error envelope.
    fn openrouter_429_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "error": {
                "code": 429,
                "message": "Rate limit exceeded for provider OpenAI. Please retry.",
                "metadata": {
                    "provider_name": "OpenAI",
                    "raw": { "error": { "message": "rate limited" } }
                }
            }
        }))
        .unwrap()
    }

    /// Spawn a mock returning a canned JSON body for any POST path.
    async fn spawn_mock(body: Vec<u8>) -> String {
        let app = Router::new().route(
            "/{*path}",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    body.clone(),
                )
            }),
        );
        spawn_and_url(app).await
    }

    /// Spawn a mock returning a canned status + body (for error envelopes).
    async fn spawn_mock_status(status: StatusCode, body: Vec<u8>) -> String {
        let app = Router::new().route(
            "/{*path}",
            post(move || async move {
                (
                    status,
                    [(header::CONTENT_TYPE, "application/json")],
                    body.clone(),
                )
            }),
        );
        spawn_and_url(app).await
    }

    /// Spawn a mock returning OpenRouter-style streaming chunks (with `provider`,
    /// `native_finish_reason`, and a final usage chunk carrying `cost`).
    async fn spawn_mock_openrouter_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default().data(r#"{"id":"gen-stream-001","provider":"OpenAI","model":"openai/gpt-4o","object":"chat.completion.chunk","created":1719000000,"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null,"native_finish_reason":null}]}"#),
                Event::default().data(r#"{"id":"gen-stream-001","provider":"OpenAI","model":"openai/gpt-4o","object":"chat.completion.chunk","created":1719000000,"choices":[{"index":0,"delta":{"content":"Hi from OpenRouter!"},"finish_reason":null,"native_finish_reason":null}]}"#),
                Event::default().data(r#"{"id":"gen-stream-001","provider":"OpenAI","model":"openai/gpt-4o","object":"chat.completion.chunk","created":1719000000,"choices":[{"index":0,"delta":{},"finish_reason":"stop","native_finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20,"cost":0.000123}}"#),
                Event::default().data("[DONE]"),
            ];
            let stream =
                futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        spawn_and_url(app).await
    }

    /// Spawn a mock that captures the request headers, returning the canned body.
    /// Used to prove OpenRouter app headers (`HTTP-Referer`, `X-Title`) reach the
    /// upstream.
    async fn spawn_mock_with_header_capture(
        body: Vec<u8>,
    ) -> (String, Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>>) {
        let captured: Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured_cl = Arc::clone(&captured);
        let app = Router::new().route(
            "/{*path}",
            post(move |req: Request<Body>| {
                let captured = Arc::clone(&captured_cl);
                let body = body.clone();
                async move {
                    {
                        let mut g = captured.lock().unwrap();
                        *g = Some(req.headers().clone());
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        body.clone(),
                    )
                }
            }),
        );
        let url = spawn_and_url(app).await;
        (url, captured)
    }

    /// Spawn a mock that captures the request body the proxy forwards upstream,
    /// returning the canned response. Used to prove the request encode path drops
    /// no input content.
    async fn spawn_mock_with_body_capture(
        body: Vec<u8>,
    ) -> (String, Arc<tokio::sync::Mutex<Option<bytes::Bytes>>>) {
        let captured: Arc<tokio::sync::Mutex<Option<bytes::Bytes>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let captured_cl = Arc::clone(&captured);
        let app = Router::new().route(
            "/{*path}",
            post(move |req: Request<Body>| {
                let captured = Arc::clone(&captured_cl);
                let body = body.clone();
                async move {
                    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    *captured.lock().await = Some(bytes);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        body.clone(),
                    )
                }
            }),
        );
        let url = spawn_and_url(app).await;
        (url, captured)
    }

    /// Bind a catch-all mock on an ephemeral port, spawn it, wait for readiness,
    /// and return its base URL.
    async fn spawn_and_url(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{addr}/v1/chat/completions")
    }

    // ===========================================================================
    // AppState + request builders
    // ===========================================================================

    /// Build an AppState whose single provider `openrouter` speaks the OpenAI Chat
    /// Completions protocol at `mock_endpoint`. Both client routes
    /// (`chat_completions` and `messages`) resolve to that one adapter, which is
    /// what enables cross-protocol translation: an Anthropic Messages client and an
    /// OpenAI Chat client both target the same OpenAI/OpenRouter upstream.
    fn openrouter_state(mock_endpoint: &str, extra_headers: HashMap<String, String>) -> AppState {
        let provider = ProviderConfig {
            name: "openrouter".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: mock_endpoint.to_owned(),
                        headers: Arc::new(extra_headers),
                    },
                );
                m
            },
            // An OpenAI-protocol provider serves both client protocols.
            routes: llm_proxy_core::ProviderRoutesConfig {
                chat_completions: Some("chat".to_owned()),
                messages: Some("chat".to_owned()),
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::new(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");

        let app_config = AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: Duration::from_secs(300),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "test-proxy".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: Duration::from_millis(500),
                log_format: Default::default(),
            },
            auth: Default::default(),
            storage: None,
        };

        AppState::new(
            app_config,
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

    fn openai_chat_request(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/providers/openrouter/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn anthropic_messages_request(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/providers/openrouter/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn make_chat_body(stream: bool) -> String {
        json!({
            "model": "openai/gpt-4o",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": stream
        })
        .to_string()
    }

    fn make_messages_body(stream: bool) -> String {
        json!({
            "model": "openai/gpt-4o",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64,
            "stream": stream
        })
        .to_string()
    }

    /// Read a non-streaming JSON response body.
    async fn read_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Collect a streaming response body as a UTF-8 string.
    async fn read_stream_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    // ===========================================================================
    // Goal: protocol hub — OpenAI client passthrough to an OpenAI/OpenRouter upstream
    // ===========================================================================

    /// An OpenAI Chat client talking to the proxy receives an OpenAI-shaped
    /// response back: `object=chat.completion`, translated text, normalised
    /// `finish_reason`, and usage with OpenAI token field names.
    #[tokio::test]
    async fn openai_client_to_openrouter_translates_to_openai_shape() {
        let mock_url = spawn_mock(openrouter_non_stream_response()).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let resp = app
            .oneshot(openai_chat_request(&make_chat_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "OpenAI client path must succeed"
        );

        let body = read_json(resp).await;
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["choices"][0]["message"]["role"], "assistant");
        assert!(
            body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("Hello from OpenRouter!"),
            "text must survive the round trip: {}",
            body
        );
        // Usage keeps OpenAI field names.
        assert_eq!(body["usage"]["prompt_tokens"], 12);
        assert_eq!(body["usage"]["completion_tokens"], 8);
    }

    // ===========================================================================
    // Goal: cross-protocol translation (the headline feature)
    // ===========================================================================

    /// An Anthropic Messages client talking to the proxy — whose upstream is an
    /// OpenAI/OpenRouter endpoint — receives an Anthropic-shaped response: the
    /// upstream OpenAI body is decoded to the core type and re-encoded for the
    /// Anthropic client. Text, `stop_reason` (`stop` -> `end_turn`), and usage
    /// (`completion_tokens` -> `output_tokens`) must all translate.
    #[tokio::test]
    async fn anthropic_client_to_openrouter_translates_to_anthropic_shape() {
        let mock_url = spawn_mock(openrouter_non_stream_response()).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "cross-protocol path must succeed"
        );

        let body = read_json(resp).await;
        // Anthropic envelope.
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        // Text translated through the OpenAI -> core -> Anthropic path.
        assert!(
            body["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Hello from OpenRouter!"),
            "text must survive cross-protocol translation: {}",
            body
        );
        // OpenAI finish_reason "stop" normalises to Anthropic "end_turn".
        assert_eq!(body["stop_reason"], "end_turn");
        // Usage field names translate to the Anthropic convention.
        assert_eq!(body["usage"]["input_tokens"], 12);
        assert_eq!(body["usage"]["output_tokens"], 8);
    }

    // ===========================================================================
    // Goal: tool-call fidelity
    // ===========================================================================

    /// An OpenAI client receives upstream `tool_calls` intact (id, function name,
    /// JSON arguments), with `finish_reason=tool_calls`.
    #[tokio::test]
    async fn tool_calls_preserved_for_openai_client() {
        let mock_url = spawn_mock(openrouter_tool_call_response()).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let body = json!({
            "model": "openai/gpt-4o",
            "messages": [{ "role": "user", "content": "weather in SF?" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }
            }]
        });
        let resp = app
            .oneshot(openai_chat_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = read_json(resp).await;
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        let tool_calls = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("tool_calls must be an array");
        assert!(!tool_calls.is_empty());
        assert_eq!(tool_calls[0]["id"], "call_or_abc123");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            r#"{"city":"San Francisco"}"#
        );
    }

    /// An Anthropic client receives upstream OpenAI tool calls translated into
    /// Anthropic `tool_use` content blocks: `type=tool_use`, the function `name`,
    /// and parsed `input` (arguments string -> object).
    #[tokio::test]
    async fn tool_calls_translate_to_anthropic_tool_use_blocks() {
        let mock_url = spawn_mock(openrouter_tool_call_response()).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = read_json(resp).await;
        assert_eq!(body["type"], "message");
        assert_eq!(body["stop_reason"], "tool_use");

        let blocks = body["content"]
            .as_array()
            .expect("content must be an array of blocks");
        let tool_use = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("at least one content block must be a tool_use")
            .clone();
        assert_eq!(tool_use["name"], "get_weather");
        // The OpenAI arguments JSON string is parsed into an Anthropic input object.
        assert_eq!(tool_use["input"]["city"], "San Francisco");
    }

    // ===========================================================================
    // Goal: robustness to provider-specific response fields
    // ===========================================================================

    /// OpenRouter responses carry fields the proxy does not model (`gen-` ids,
    /// top-level `provider`, `cost` inside `usage`, `system_fingerprint`,
    /// `native_finish_reason`, `logprobs`, `refusal`). The OpenAI response structs
    /// deliberately omit `deny_unknown_fields` (see the doc on `ChatCompletionResponse`
    /// in `llm-proxy-protocol/src/openai.rs`), so these must not surface as a 502 to
    /// either client protocol. This test pins that contract.
    #[tokio::test]
    async fn openrouter_specific_extras_do_not_break_conversion() {
        let mock_url = spawn_mock(openrouter_non_stream_response()).await;

        // OpenAI client.
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(openai_chat_request(&make_chat_body(false)))
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "OpenRouter extras must not break OpenAI-client conversion"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "chat.completion");

        // Anthropic client (cross-protocol) — same upstream body, different decode.
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "OpenRouter extras must not break cross-protocol conversion"
        );
        let v = read_json(resp).await;
        assert_eq!(v["type"], "message");
    }

    // ===========================================================================
    // Goal: streaming fidelity
    // ===========================================================================

    /// An OpenAI client streaming request yields `chat.completion.chunk` frames and
    /// terminates with `[DONE]`, with the translated text present in the deltas.
    #[tokio::test]
    async fn openai_client_stream_ends_with_done() {
        let mock_url = spawn_mock_openrouter_stream().await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let resp = app
            .oneshot(openai_chat_request(&make_chat_body(true)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_stream_text(resp).await;

        assert!(
            text.contains("\"object\":\"chat.completion.chunk\""),
            "stream must emit OpenAI chunks, got: {text}"
        );
        assert!(
            text.contains("Hi from OpenRouter!"),
            "delta text must survive streaming, got: {text}"
        );
        assert!(
            text.contains("[DONE]"),
            "OpenAI stream must end with [DONE], got: {text}"
        );
    }

    /// An Anthropic client streaming against an OpenAI/OpenRouter upstream is the
    /// cross-protocol streaming gold path: OpenAI chunks are decoded to core events
    /// and re-emitted as Anthropic SSE events (`message_start`,
    /// `content_block_delta`, `message_stop`).
    #[tokio::test]
    async fn anthropic_client_stream_cross_protocol() {
        let mock_url = spawn_mock_openrouter_stream().await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(true)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_stream_text(resp).await;

        assert!(
            text.contains("event: message_start"),
            "missing message_start, got: {text}"
        );
        assert!(
            text.contains("event: content_block_delta"),
            "missing content_block_delta, got: {text}"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop, got: {text}"
        );
        assert!(
            text.contains("Hi from OpenRouter!"),
            "delta text must survive cross-protocol streaming, got: {text}"
        );
    }

    // ===========================================================================
    // Goal: OpenRouter app-header injection
    // ===========================================================================

    /// Headers declared on the adapter (`HTTP-Referer`, `X-Title` — what OpenRouter
    /// expects from integrating apps) are forwarded on the upstream request, while
    /// the bearer API key is applied via the configured `auth_style`.
    #[tokio::test]
    async fn openrouter_app_headers_forwarded_upstream() {
        let (mock_url, captured) =
            spawn_mock_with_header_capture(openrouter_non_stream_response()).await;

        let mut headers = HashMap::new();
        headers.insert("HTTP-Referer".to_owned(), "https://example.com".to_owned());
        headers.insert("X-Title".to_owned(), "llm-proxy-e2e".to_owned());
        let app = build_router(openrouter_state(&mock_url, headers));

        let resp = app
            .oneshot(openai_chat_request(&make_chat_body(false)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Give the background mock handler a moment to record the request.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let g = captured.lock().unwrap();
        let upstream_headers = g
            .as_ref()
            .expect("upstream should have received the proxied request");

        assert_eq!(
            upstream_headers.get("http-referer").unwrap(),
            "https://example.com",
            "HTTP-Referer must be forwarded to OpenRouter"
        );
        assert_eq!(
            upstream_headers.get("x-title").unwrap(),
            "llm-proxy-e2e",
            "X-Title must be forwarded to OpenRouter"
        );
        assert!(
            upstream_headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().starts_with("Bearer "))
                .unwrap_or(false),
            "bearer auth must be applied from the configured api_key/auth_style"
        );
    }

    // ===========================================================================
    // Goal: error fidelity — upstream 429 passes through, client-shaped
    // ===========================================================================

    /// An OpenRouter 429 surfaces to *each* client protocol as a client-shaped
    /// error with the 429 *status* passed through — not collapsed to a 502. Two
    /// finer points are pinned here, both from `routes/error_response.rs`:
    ///
    /// - The error `type` is `"api_error"`, **not** `"rate_limit_error"`. The
    ///   `rate_limit_error` label is reserved for the proxy's *own* rate limiter
    ///   (`RouteError::RateLimited`); an *upstream* 429 is `RouteError::Upstream`,
    ///   which keeps status 429 (the one code `map_upstream_status` passes through)
    ///   but is typed generically.
    /// - The upstream's raw error body (OpenRouter's `provider_name`, `raw`, etc.)
    ///   is logged server-side only and **must not** reach the client envelope —
    ///   the client gets a sanitized generic message.
    #[tokio::test]
    async fn openrouter_429_surfaces_as_client_shaped_error() {
        let mock_url =
            spawn_mock_status(StatusCode::TOO_MANY_REQUESTS, openrouter_429_body()).await;

        // Anthropic client (cross-protocol error translation).
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "upstream 429 must pass through"
        );
        let body = read_json(resp).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
        let msg = body["error"]["message"]
            .as_str()
            .expect("error must carry a message");
        assert!(
            !msg.contains("provider OpenAI"),
            "upstream raw error text must not leak into the client envelope: {msg}"
        );

        // OpenAI client.
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(openai_chat_request(&make_chat_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "upstream 429 must pass through"
        );
        let body = read_json(resp).await;
        assert_eq!(body["error"]["type"], "api_error");
        let msg = body["error"]["message"]
            .as_str()
            .expect("error must carry a message");
        assert!(
            !msg.contains("provider OpenAI"),
            "upstream raw error text must not leak into the client envelope: {msg}"
        );
    }

    // ===========================================================================
    // Goal: content/token fidelity — nothing is stripped in either direction
    // ===========================================================================

    /// Input fidelity: the request the proxy forwards upstream must carry every
    /// piece of content the client sent — the system prompt, every message turn,
    /// and verbatim text including unicode/emoji/CJK. If the encode path dropped
    /// any token, one of these distinctive fragments would be missing from the
    /// captured upstream body. (Anthropic-client -> OpenAI-upstream encode path.)
    #[tokio::test]
    async fn input_content_is_preserved_on_encode() {
        let (mock_url, captured) =
            spawn_mock_with_body_capture(openrouter_non_stream_response()).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));

        let body = json!({
            "model": "openai/gpt-4o",
            "system": "You are a precise assistant. 🤖 Rule: never omit anything.",
            "messages": [
                { "role": "user", "content": "Turn one: café résumé naïve — emoji 🚀 and CJK 你好世界." },
                { "role": "assistant", "content": "Acknowledged: round-trip-marker-α." },
                { "role": "user", "content": "Turn three: the quick brown fox jumps over the lazy dog." }
            ],
            "max_tokens": 64
        });
        let resp = app
            .oneshot(anthropic_messages_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        tokio::time::sleep(Duration::from_millis(100)).await;
        let upstream = captured
            .lock()
            .await
            .take()
            .expect("upstream should have received the proxied request body");
        let up: Value =
            serde_json::from_slice(&upstream).expect("forwarded upstream body must be valid JSON");
        let joined = serde_json::to_string(&up["messages"]).expect("messages serialize");

        for fragment in [
            "You are a precise assistant",
            "🤖 Rule: never omit anything",
            "café résumé naïve",
            "🚀",
            "你好世界",
            "round-trip-marker-α",
            "the quick brown fox jumps over the lazy dog",
        ] {
            assert!(
                joined.contains(fragment),
                "encode stripped input fragment {fragment:?}\nupstream body: {joined}"
            );
        }
        let n = up["messages"].as_array().map(Vec::len).unwrap_or(0);
        assert!(
            n >= 3,
            "expected all three turns forwarded upstream, got {n} messages"
        );
    }

    /// Output fidelity: a rich OpenRouter response (long multi-paragraph text with
    /// unicode/emoji, two tool calls, and explicit usage) must convert to an
    /// Anthropic response that preserves ALL of it — full verbatim text, every
    /// tool_use block, and exact usage token counts. If the decode path stripped
    /// anything, a fragment, a tool_use block, or a token count would be wrong.
    #[tokio::test]
    async fn output_content_is_preserved_on_decode() {
        let long_text = "Para one: the quick brown fox. Para two: café résumé 🚀 你好世界. \
                     Para three: round-trip-marker-β.";
        let body = serde_json::to_vec(&json!({
            "id": "gen-out-001",
            "provider": "OpenAI",
            "model": "openai/gpt-4o",
            "object": "chat.completion",
            "created": 1719000000,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": long_text,
                    "tool_calls": [
                        { "id": "call_or_1", "type": "function",
                          "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" } },
                        { "id": "call_or_2", "type": "function",
                          "function": { "name": "get_time", "arguments": "{\"zone\":\"PST\"}" } }
                    ]
                },
                "finish_reason": "tool_calls",
                "native_finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": { "prompt_tokens": 77, "completion_tokens": 123, "total_tokens": 200 }
        }))
        .unwrap();

        let mock_url = spawn_mock(body).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let v = read_json(resp).await;
        let joined = serde_json::to_string(&v["content"]).expect("content serializes");

        // Full verbatim text — every paragraph and the unicode/emoji runs.
        for fragment in [
            "Para one: the quick brown fox",
            "café résumé 🚀 你好世界",
            "round-trip-marker-β",
        ] {
            assert!(
                joined.contains(fragment),
                "decode stripped output text fragment {fragment:?}\ncontent: {joined}"
            );
        }
        // Both tool calls survive as Anthropic tool_use blocks.
        let tool_uses = v["content"]
            .as_array()
            .map(|blocks| blocks.iter().filter(|b| b["type"] == "tool_use").count())
            .unwrap_or(0);
        assert_eq!(
            tool_uses, 2,
            "both tool calls must survive decode: {joined}"
        );
        // Usage token counts pass through unchanged (OpenAI -> Anthropic field names).
        assert_eq!(
            v["usage"]["input_tokens"], 77,
            "input token count must be preserved"
        );
        assert_eq!(
            v["usage"]["output_tokens"], 123,
            "output token count must be preserved"
        );
    }

    /// Robustness: real OpenRouter (and other OpenAI-compatible) 2xx responses can
    /// omit `usage` entirely (e.g. certain Venice-backed free models). Since
    /// `ChatCompletionResponse.usage` is `#[serde(default)]`, an absent `usage`
    /// decodes with zero token counts instead of failing the whole response into a
    /// 502 "provider response decode error". This previously reproduced the
    /// intermittent live 502 for `meta-llama/llama-3.2-3b-instruct:free`.
    #[tokio::test]
    async fn response_missing_usage_decodes_with_zero_usage() {
        let body = serde_json::to_vec(&json!({
            "id": "gen-nousage",
            "provider": "Venice",
            "model": "meta-llama/llama-3.2-3b-instruct:free",
            "object": "chat.completion",
            "created": 1719000000,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "pong" },
                "finish_reason": "stop"
            }]
            // NOTE: deliberately no "usage" field — some OpenRouter backends omit it.
        }))
        .unwrap();

        let mock_url = spawn_mock(body).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a 200 body missing `usage` must decode (with zero usage), not 502"
        );
        let v = read_json(resp).await;
        assert_eq!(v["type"], "message");
        assert_eq!(v["usage"]["input_tokens"], 0);
        assert_eq!(v["usage"]["output_tokens"], 0);
        assert!(
            v["content"][0]["text"].as_str().unwrap().contains("pong"),
            "text content must still come through"
        );
    }

    /// Variant where the provider sends `"usage": null` instead of omitting the
    /// field. Must also decode with zero usage, not 502.
    #[tokio::test]
    async fn response_with_null_usage_decodes_with_zero_usage() {
        let body = serde_json::to_vec(&json!({
            "id": "gen-nullusage",
            "provider": "Venice",
            "model": "meta-llama/llama-3.2-3b-instruct:free",
            "object": "chat.completion",
            "created": 1719000000,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "pong" },
                "finish_reason": "stop"
            }],
            "usage": null
        }))
        .unwrap();

        let mock_url = spawn_mock(body).await;
        let app = build_router(openrouter_state(&mock_url, HashMap::new()));
        let resp = app
            .oneshot(anthropic_messages_request(&make_messages_body(false)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a 200 body with `usage: null` must decode (with zero usage), not 502"
        );
        let v = read_json(resp).await;
        assert_eq!(v["type"], "message");
        assert_eq!(v["usage"]["input_tokens"], 0);
        assert_eq!(v["usage"]["output_tokens"], 0);
    }

    // ===========================================================================
    // Goal: live network validation (opt-in)
    // ===========================================================================

    /// Default candidate free models. OpenRouter rate-limits its free tier
    /// aggressively, so the live test tries several and requires at least one to
    /// succeed. Override with `OPENROUTER_MODELS=m1,m2,...` (or a single
    /// `OPENROUTER_MODEL=m`). Names are current as of authoring time; OpenRouter
    /// rotates free models, so a missing one is reported and skipped rather than
    /// failing the run.
    const DEFAULT_FREE_MODELS: &[&str] = &[
        "meta-llama/llama-3.2-3b-instruct:free",
        "liquid/lfm-2.5-1.2b-instruct:free",
        "google/gemma-4-26b-a4b-it:free",
        "openai/gpt-oss-20b:free",
        "qwen/qwen3-coder:free",
        "meta-llama/llama-3.3-70b-instruct:free",
        "nvidia/nemotron-nano-9b-v2:free",
        "cohere/north-mini-code:free",
    ];

    /// Build an AppState whose single `openrouter` provider targets the real
    /// OpenRouter Chat Completions endpoint, applying the `HTTP-Referer` / `X-Title`
    /// headers OpenRouter expects from integrating apps.
    fn live_state(api_key: &str) -> AppState {
        let mut headers = HashMap::new();
        headers.insert(
            "HTTP-Referer".to_owned(),
            "https://github.com/llm-proxy".to_owned(),
        );
        headers.insert("X-Title".to_owned(), "llm-proxy live e2e".to_owned());

        let provider = ProviderConfig {
            name: "openrouter".to_owned(),
            api_key: secrecy::SecretString::from(api_key),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: "https://openrouter.ai/api/v1/chat/completions".to_owned(),
                        headers: Arc::new(headers),
                    },
                );
                m
            },
            routes: llm_proxy_core::ProviderRoutesConfig {
                chat_completions: Some("chat".to_owned()),
                messages: Some("chat".to_owned()),
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::new(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let app_config = AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: Duration::from_secs(60),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "test-proxy".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: Duration::from_millis(500),
                log_format: Default::default(),
            },
            auth: Default::default(),
            storage: None,
        };
        AppState::new(
            app_config,
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

    /// Issue one live Anthropic Messages request, retrying with backoff while
    /// the upstream answers 429.
    ///
    /// Returns the final status and raw response body.
    async fn live_request_with_backoff(
        app: &axum::Router,
        model: &str,
        body: &str,
        max_attempts: u32,
        backoff: Duration,
    ) -> (StatusCode, Vec<u8>) {
        let mut status = StatusCode::OK;
        let mut bytes = Vec::<u8>::new();
        for attempt in 1..=max_attempts {
            let resp = app
                .clone()
                .oneshot(anthropic_messages_request(body))
                .await
                .unwrap();
            status = resp.status();
            bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < max_attempts {
                eprintln!(
                    "[{model}] attempt {attempt}/{max_attempts}: 429 - backing off {backoff:?}, retrying"
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
            break;
        }
        (status, bytes)
    }

    /// Real end-to-end round trips against <https://openrouter.ai> through the proxy.
    ///
    /// Skipped unless `OPENROUTER_API_KEY` is set; run with:
    ///
    /// ```text
    /// OPENROUTER_API_KEY=sk-or-... \
    ///   cargo test -p llm-proxy-server --test openrouter_e2e \
    ///   live_openrouter_round_trip -- --ignored --nocapture
    /// ```
    ///
    /// Tries several free models (OpenRouter rate-limits the free tier, so some
    /// will 429) and requires at least one to complete a full Anthropic-client →
    /// OpenRouter cross-protocol conversion. A 502 from any model is a hard failure
    /// (it would mean the proxy could not convert a real OpenRouter response);
    /// 429/400/404/etc. are reported but tolerated. Override the set with
    /// `OPENROUTER_MODELS=a,b,c` or pin a single `OPENROUTER_MODEL`.
    #[tokio::test]
    #[ignore = "requires network + OPENROUTER_API_KEY; run with --ignored"]
    async fn live_openrouter_round_trip() {
        let api_key = match std::env::var("OPENROUTER_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("skipping: OPENROUTER_API_KEY not set");
                return;
            }
        };

        let candidates: Vec<String> = if let Ok(one) = std::env::var("OPENROUTER_MODEL") {
            vec![one]
        } else if let Ok(list) = std::env::var("OPENROUTER_MODELS") {
            list.split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            DEFAULT_FREE_MODELS.iter().map(|s| s.to_string()).collect()
        };

        let app = build_router(live_state(&api_key));

        // OpenRouter rate-limits the free tier per-minute (and per-day for some
        // models), so each model is retried with backoff on 429. Override with
        // OPENROUTER_MAX_ATTEMPTS / OPENROUTER_BACKOFF_MS.
        let max_attempts = std::env::var("OPENROUTER_MAX_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3u32);
        let backoff = Duration::from_millis(
            std::env::var("OPENROUTER_BACKOFF_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15_000),
        );

        let mut ok = 0usize;
        let mut rate_limited = 0usize;
        let mut other = 0usize;
        let mut upstream_failures = 0usize;
        let mut first_ok: Option<String> = None;
        // A 502 is only a *proxy* bug when it is a ProviderDecode
        // ("provider response decode error"); a 502 carrying "upstream service
        // error" is an upstream 5xx the proxy correctly collapsed (the upstream
        // provider's fault — common with free-tier overload) and is tolerated.
        // Asserted empty at the end; run with RUST_LOG=llm_proxy_server=debug to
        // see the underlying serde cause of any real decode failure.
        let mut decode_failures: Vec<(String, String)> = Vec::new();

        for model in &candidates {
            // Anthropic Messages client -> OpenRouter (OpenAI) upstream: the
            // full cross-protocol translation against the real service.
            let body = json!({
                "model": model,
                "messages": [{ "role": "user", "content": "Reply with exactly the word: pong" }],
                "max_tokens": 16,
                "stream": false
            });
            let (status, bytes) =
                live_request_with_backoff(&app, model, &body.to_string(), max_attempts, backoff)
                    .await;
            let text = String::from_utf8_lossy(&bytes).into_owned();

            eprintln!("[{model}] final status={status} body={text}");

            // Distinguish the two 502 causes. Only a ProviderDecode ("provider
            // response decode error") is the proxy's fault; an "upstream service
            // error" 502 is an upstream 5xx the proxy correctly collapsed and is
            // tolerated.
            match status {
                StatusCode::OK => {
                    let v: Value = serde_json::from_slice(&bytes)
                        .unwrap_or_else(|_| panic!("[{model}] OK body must be valid JSON: {text}"));
                    assert_eq!(
                        v["type"], "message",
                        "[{}] live response must be Anthropic-shaped (type=message): {}",
                        model, text
                    );
                    assert!(
                        v["content"].is_array(),
                        "[{}] live response must carry Anthropic content blocks: {}",
                        model,
                        text
                    );
                    ok += 1;
                    first_ok.get_or_insert_with(|| text.clone());
                }
                StatusCode::TOO_MANY_REQUESTS => rate_limited += 1,
                StatusCode::BAD_GATEWAY if text.contains("provider response decode error") => {
                    decode_failures.push((model.clone(), text.clone()));
                }
                StatusCode::BAD_GATEWAY => upstream_failures += 1,
                _ => other += 1,
            }
        }

        eprintln!(
            "\nsummary: {} converted ok, {} rate-limited, {} upstream failures (502, upstream's fault), {} other errors, {} decode failures (502, proxy bug) across {} candidates",
            ok,
            rate_limited,
            upstream_failures,
            other,
            decode_failures.len(),
            candidates.len()
        );
        assert!(
            ok > 0,
            "no free model completed a successful cross-protocol conversion \
         (rate_limited={rate_limited}, other={other}); retry later or set OPENROUTER_MODELS"
        );
        assert!(
            decode_failures.is_empty(),
            "{} model(s) triggered a proxy decode failure (502) — a real conversion bug; \
         run with RUST_LOG=llm_proxy_server=debug --nocapture to see the serde cause: {:#?}",
            decode_failures.len(),
            decode_failures
        );
        if let Some(body) = first_ok {
            eprintln!("\nfirst successful Anthropic-shaped response:\n{body}");
        }
    }

    /// Live output fidelity: a real model is asked to emit a short, distinctive
    /// multi-token sequence (six planet names). The converted Anthropic response
    /// must contain ALL of them — if the decode/re-encode path dropped any output
    /// token, a word would be missing. Reasoning-only responses (no text block,
    /// common when a thinking model is truncated) are skipped rather than failed.
    /// Retries rate-limited models once. Skipped without `OPENROUTER_API_KEY`.
    #[tokio::test]
    #[ignore = "requires network + OPENROUTER_API_KEY; run with --ignored"]
    async fn live_output_completeness_no_token_loss() {
        let api_key = match std::env::var("OPENROUTER_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("skipping: OPENROUTER_API_KEY not set");
                return;
            }
        };

        let candidates: Vec<String> = if let Ok(list) = std::env::var("OPENROUTER_MODELS") {
            list.split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            DEFAULT_FREE_MODELS.iter().map(|s| s.to_string()).collect()
        };

        let words = ["mercury", "venus", "earth", "mars", "jupiter", "saturn"];
        let app = build_router(live_state(&api_key));

        let mut verified = false;
        for model in &candidates {
            let body = json!({
                "model": model,
                "messages": [{
                    "role": "user",
                    "content": "Respond with exactly this line and nothing else: mercury venus earth mars jupiter saturn"
                }],
                "max_tokens": 64,
                "stream": false
            });

            // One retry on rate limit, with a short backoff.
            let (status, bytes) = live_request_with_backoff(
                &app,
                model,
                &body.to_string(),
                2,
                Duration::from_secs(15),
            )
            .await;
            if status != StatusCode::OK {
                eprintln!("[{model}] status={status}; skipping output-completeness check");
                continue;
            }

            let full = String::from_utf8_lossy(&bytes);
            let v: Value = serde_json::from_slice(&bytes).expect("OK body must be valid JSON");
            // Scan only text blocks — ignore tool_use ids / thinking that may carry
            // unrelated tokens.
            let text_only: String = v["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            eprintln!("[{model}] text block: {text_only}");

            if text_only.trim().is_empty() {
                eprintln!("[{model}] no text block (reasoning-only response) — trying next model");
                continue;
            }

            let lower = text_only.to_lowercase();
            let missing: Vec<&str> = words
                .iter()
                .copied()
                .filter(|w| !lower.contains(*w))
                .collect();
            assert!(
                missing.is_empty(),
                "[{model}] conversion dropped output tokens; missing words: {:?}\nfull response: {full}",
                missing
            );
            let out_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
            assert!(out_tokens > 0, "[{model}] output_tokens must be non-zero");
            eprintln!("[{model}] ✓ all six words present in output, output_tokens={out_tokens}");
            verified = true;
            break;
        }

        assert!(
            verified,
            "no free model returned a text response to verify output completeness; \
         retry later or set OPENROUTER_MODELS"
        );
    }
}
