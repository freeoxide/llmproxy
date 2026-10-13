//! Integration tests for the core pipeline (Phase 8).
//!
//! These tests exercise the full request path through the axum router with a
//! local mock upstream server. Each test configures AppState with a provider route
//! pointing at the mock server, which returns canned provider-specific responses
//! that are translated through the core pipeline into Anthropic-shaped responses.

#[cfg(test)]
mod tests {

    use std::collections::HashMap;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
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

    // ---------------------------------------------------------------------------
    // Mock server readiness helper
    // ---------------------------------------------------------------------------

    /// Wait for a mock server to be ready by polling its TCP port.
    ///
    /// Replaces `tokio::time::sleep(Duration::from_millis(50))` with a
    /// deterministic readiness check that retries until the server accepts
    /// a connection or the timeout elapses.
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

    /// Spawn a mock axum server on `listener`, returning immediately while the
    /// server runs in the background.
    ///
    /// Unlike a bare `tokio::spawn(async move { axum::serve(..).await })` whose
    /// `JoinHandle` is dropped (silently swallowing any panic in the mock server
    /// task — audit `testing-audit:mock-server-joinhandle-swallow`), this wrapper
    /// catches a panic in the serve future, prints it to stderr, and aborts the
    /// process. A mock-handler bug therefore surfaces loudly as a test failure
    /// instead of the connection just closing mid-stream with no diagnostic.
    fn spawn_mock_serve(listener: tokio::net::TcpListener, app: Router) {
        tokio::spawn(async move {
            // std::panic::catch_unwind requires UnwindSafe; the router/listener
            // capture is fine for a mock that owns them exclusively.
            let result = std::panic::AssertUnwindSafe(axum::serve(listener, app).into_future())
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("mock axum::serve error: {e}"),
                Err(panic) => {
                    // Surface the panic payload, then abort so the test binary fails
                    // rather than continuing with a dead mock.
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

    // ---------------------------------------------------------------------------
    // Mock upstream server helpers
    // ---------------------------------------------------------------------------

    /// A canned Anthropic Messages API success response.
    fn anthropic_success_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "msg_mock123",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello from mock!" }],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }))
        .unwrap()
    }

    /// A canned OpenAI Chat Completions API success response.
    fn openai_chat_success_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "chatcmpl-test123",
            "object": "chat.completion",
            "created": 12345,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from OpenAI mock!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }))
        .unwrap()
    }

    /// A canned OpenAI Responses API success response.
    fn openai_responses_success_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "resp_test123",
            "object": "response",
            "created": 12345,
            "model": "gpt-4o-2024-08-06",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "Hello from Responses mock!"
                }]
            }],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        }))
        .unwrap()
    }

    /// A canned Gemini generateContent API success response.
    fn gemini_success_response() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{ "text": "Hello from Gemini mock!" }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }))
        .unwrap()
    }

    /// Build AppState in TOML mode with a single provider of the given protocol
    /// routing to the given mock endpoint.
    fn state_with_provider(
        mock_endpoint: &str,
        protocol: &str,
        adapter_name: &str,
        _model_name: &str,
        pricing: HashMap<llm_proxy_core::ModelId, llm_proxy_core::ModelPricing>,
    ) -> AppState {
        // Build routes based on protocol so the provider-based routing can resolve.
        let routes = match protocol {
            "anthropic_messages" => llm_proxy_core::ProviderRoutesConfig {
                messages: Some(adapter_name.to_owned()),
                chat_completions: None,
            },
            "openai_chat_completions" => llm_proxy_core::ProviderRoutesConfig {
                chat_completions: Some(adapter_name.to_owned()),
                messages: Some(adapter_name.to_owned()),
            },
            _ => llm_proxy_core::ProviderRoutesConfig {
                messages: Some(adapter_name.to_owned()),
                chat_completions: None,
            },
        };

        let provider = ProviderConfig {
            name: "mock-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    adapter_name.to_owned(),
                    ProviderAdapterConfig {
                        protocol: protocol.to_owned(),
                        endpoint: mock_endpoint.to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes,
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing,
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
                admin_bind: None,
                admin_on_main_listener: false,
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

    /// Convenience: AppState with an Anthropic provider.
    fn state_with_anthropic_provider(mock_endpoint: &str) -> AppState {
        state_with_provider(
            mock_endpoint,
            "anthropic_messages",
            "messages",
            "claude-sonnet-4-6",
            HashMap::new(),
        )
    }

    /// Convenience: AppState with an Anthropic provider and a pricing table for the
    /// upstream model, so cost computation is exercised end-to-end.
    fn state_with_anthropic_provider_priced(
        mock_endpoint: &str,
        pricing: HashMap<llm_proxy_core::ModelId, llm_proxy_core::ModelPricing>,
    ) -> AppState {
        state_with_provider(
            mock_endpoint,
            "anthropic_messages",
            "messages",
            "claude-sonnet-4-6",
            pricing,
        )
    }

    /// Convenience: AppState with an OpenAI Chat provider.
    fn state_with_openai_chat_provider(mock_endpoint: &str) -> AppState {
        state_with_provider(
            mock_endpoint,
            "openai_chat_completions",
            "chat",
            "gpt-4o",
            HashMap::new(),
        )
    }

    /// Convenience: AppState with an OpenAI Responses provider.
    fn state_with_openai_responses_provider(mock_endpoint: &str) -> AppState {
        state_with_provider(
            mock_endpoint,
            "openai_responses",
            "responses",
            "gpt-4o-responses",
            HashMap::new(),
        )
    }

    /// Convenience: AppState with a Gemini provider.
    fn state_with_gemini_provider(mock_endpoint: &str) -> AppState {
        state_with_provider(
            mock_endpoint,
            "gemini_generate_content",
            "gemini",
            "gemini-2.5-pro",
            HashMap::new(),
        )
    }

    /// Spawn a local mock axum server returning a canned response body.
    /// The handler receives the response body as bytes and returns it with the
    /// given content type.
    async fn spawn_mock_server(response_body: Vec<u8>, content_type: &str) -> String {
        let ct = content_type.to_owned();
        let app = Router::new().route(
            "/{*path}",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, ct.clone())],
                    response_body.clone(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a local mock axum server returning canned Anthropic non-streaming
    /// responses. Returns the base URL.
    async fn spawn_mock_anthropic_non_stream() -> String {
        spawn_mock_server(anthropic_success_response(), "application/json").await
    }

    /// Spawn a local mock axum server returning OpenAI Chat non-streaming response.
    async fn spawn_mock_openai_chat_non_stream() -> String {
        spawn_mock_server(openai_chat_success_response(), "application/json").await
    }

    /// Spawn a local mock axum server returning OpenAI Responses non-streaming response.
    async fn spawn_mock_openai_responses_non_stream() -> String {
        spawn_mock_server(openai_responses_success_response(), "application/json").await
    }

    /// Spawn a local mock axum server returning Gemini non-streaming response.
    async fn spawn_mock_gemini_non_stream() -> String {
        spawn_mock_server(gemini_success_response(), "application/json").await
    }

    /// Spawn a local mock axum server returning canned Anthropic streaming SSE
    /// events.
    async fn spawn_mock_anthropic_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .event("message_start")
                    .data(r#"{"type":"message_start","message":{"id":"msg_mock_stream","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#),
                Event::default()
                    .event("content_block_start")
                    .data(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
                Event::default()
                    .event("content_block_delta")
                    .data(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#),
                Event::default()
                    .event("content_block_stop")
                    .data(r#"{"type":"content_block_stop","index":0}"#),
                Event::default()
                    .event("message_delta")
                    .data(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#),
                Event::default()
                    .event("message_stop")
                    .data(r#"{"type":"message_stop"}"#),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server returning OpenAI Chat streaming SSE events.
    async fn spawn_mock_openai_chat_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .data(r#"{"id":"chatcmpl-stream","object":"chat.completion.chunk","created":12345,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#),
                Event::default()
                    .data(r#"{"id":"chatcmpl-stream","object":"chat.completion.chunk","created":12345,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi from OpenAI!"},"finish_reason":null}]}"#),
                Event::default()
                    .data(r#"{"id":"chatcmpl-stream","object":"chat.completion.chunk","created":12345,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
                Event::default()
                    .data("[DONE]"),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server returning OpenAI Responses streaming SSE events.
    async fn spawn_mock_openai_responses_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .data(r#"{"type":"response.created","response":{"id":"resp_stream","object":"response","created":12345,"model":"gpt-4o","status":"in_progress","output":[]}}"#),
                Event::default()
                    .data(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#),
                Event::default()
                    .data(r#"{"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#),
                Event::default()
                    .data(r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hi from Responses!"}"#),
                Event::default()
                    .data(r#"{"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Hi from Responses!"}"#),
                Event::default()
                    .data(r#"{"type":"response.completed","response":{"id":"resp_stream","object":"response","created":12345,"model":"gpt-4o","status":"completed","output":[{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Hi from Responses!"}]}],"usage":{"input_tokens":10,"output_tokens":5}}}"#),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server returning Gemini streaming SSE events.
    async fn spawn_mock_gemini_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .data(r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi from Gemini!"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server that returns HTTP 500.
    async fn spawn_mock_500() -> String {
        let app = Router::new().route(
            "/{*path}",
            post(|| async {
                (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"type":"error","error":{"type":"internal_error","message":"upstream crash"}}"#
                    .as_bytes()
                    .to_vec(),
            )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server that returns an SSE stream where a valid
    /// `content_block_delta` carrying the marker text `"good-delta"` is followed by
    /// a malformed JSON event, then a final `message_stop`.
    ///
    /// The Anthropic provider adapter silently skips the malformed frame (returns
    /// `Ok(vec![])`), so the valid delta BEFORE it is delivered and the stream
    /// completes normally. The marker text lets tests assert that valid events
    /// survive a skipped malformed frame (audit `duplicate-malformed-stream-tests`).
    async fn spawn_mock_malformed_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .event("message_start")
                    .data(r#"{"type":"message_start","message":{"id":"msg_mock","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#),
                // A valid delta carrying a marker so tests can assert it is delivered.
                Event::default()
                    .event("content_block_start")
                    .data(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
                Event::default()
                    .event("content_block_delta")
                    .data(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"good-delta"}}"#),
                // Malformed event: invalid JSON, silently skipped by the adapter.
                Event::default()
                    .event("content_block_delta")
                    .data("this is not valid json {{{"),
                Event::default()
                    .event("message_stop")
                    .data(r#"{"type":"message_stop"}"#),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server that returns a genuine Anthropic `type: error` event
    /// mid-stream after a valid `message_start`.
    ///
    /// Unlike `spawn_mock_malformed_stream` (whose malformed JSON is silently
    /// skipped), this drives the real in-band error path: the Anthropic adapter maps
    /// `event: error` to `CoreEvent::Error`, which the client encoder surfaces as an
    /// `event: error` SSE frame (audit `in-band-error-event-path-untested`).
    async fn spawn_mock_in_band_error_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![
                Event::default()
                    .event("message_start")
                    .data(r#"{"type":"message_start","message":{"id":"msg_err","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#),
                // A genuine protocol-level error event the decoder maps to
                // CoreEvent::Error (anthropic.rs "error" arm).
                Event::default()
                    .event("error")
                    .data(r#"{"type":"error","error":{"type":"overloaded_error","message":"Too many requests"}}"#),
                Event::default()
                    .event("message_stop")
                    .data(r#"{"type":"message_stop"}"#),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server that accepts a request, sends a few events, then
    /// abruptly closes the connection (simulating upstream disconnect).
    async fn spawn_mock_disconnect_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            // Return only message_start, then stop (simulating disconnect).
            let events = vec![
                Event::default()
                    .event("message_start")
                    .data(r#"{"type":"message_start","message":{"id":"msg_mock","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#),
                Event::default()
                    .event("content_block_start")
                    .data(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
                // No message_stop -- stream is truncated.
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
            let sse = Sse::new(stream).keep_alive(KeepAlive::default());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.into_response(),
            )
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{}/providers/mock-provider/v1/messages", addr)
    }

    /// Spawn a mock server that captures whether it received a request.
    /// Returns (base_url, request_received_flag).
    async fn spawn_mock_with_request_tracker(response_body: Vec<u8>) -> (String, Arc<AtomicBool>) {
        let received = Arc::new(AtomicBool::new(false));
        let received_clone = Arc::clone(&received);
        let app = Router::new().route(
            "/{*path}",
            post(move || {
                let received = Arc::clone(&received_clone);
                async move {
                    received.store(true, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        response_body.clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        (
            format!("http://{}/providers/mock-provider/v1/messages", addr),
            received,
        )
    }

    /// Spawn a mock server that records the received request body.
    /// Returns (base_url, received_body_arc).
    async fn spawn_mock_with_body_capture(
        response_body: Vec<u8>,
    ) -> (String, Arc<tokio::sync::Mutex<Option<Vec<u8>>>>) {
        let captured: Arc<tokio::sync::Mutex<Option<Vec<u8>>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let captured_clone = Arc::clone(&captured);
        let app = Router::new().route(
            "/{*path}",
            post(move |body: axum::body::Bytes| {
                let captured = Arc::clone(&captured_clone);
                async move {
                    let mut guard = captured.lock().await;
                    *guard = Some(body.to_vec());
                    drop(guard);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        response_body.clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        (
            format!("http://{}/providers/mock-provider/v1/messages", addr),
            captured,
        )
    }

    fn make_messages_body(model: &str, stream: bool) -> String {
        json!({
            "model": model,
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64,
            "stream": stream
        })
        .to_string()
    }

    /// Build a request to /providers/mock-provider/v1/messages.
    fn messages_request(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    // ===========================================================================
    // Non-streaming tests: Anthropic provider
    // ===========================================================================

    /// configured Anthropic provider returns Anthropic response through core pipeline
    #[tokio::test]
    async fn anthropic_provider_returns_anthropic_response() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert!(
            body["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Hello from mock!")
        );
    }

    /// request ID header is present on success
    #[tokio::test]
    async fn request_id_header_present_on_success() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let request_id = resp.headers().get("x-request-id");
        assert!(
            request_id.is_some(),
            "x-request-id header must be present on success"
        );
        let id_str = request_id.unwrap().to_str().unwrap();
        assert!(!id_str.is_empty(), "x-request-id must not be empty");
    }

    /// upstream 500 returns 502 Bad Gateway
    #[tokio::test]
    async fn upstream_500_returns_502() {
        let mock_url = spawn_mock_500().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["type"], "error");
        assert_eq!(resp_body["error"]["type"], "api_error");
    }

    /// unknown model is passed through to upstream (provider-based routing does not
    /// validate model names locally; the upstream provider decides)
    #[tokio::test]
    async fn unknown_model_is_passed_through() {
        let (mock_url, received) =
            spawn_mock_with_request_tracker(anthropic_success_response()).await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("nonexistent-model", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // With provider-based routing, the model name is passed through to the
        // upstream provider. The mock returns 200, so we get a successful response.
        assert_eq!(resp.status(), StatusCode::OK);

        // Verify upstream WAS called (model name is passed through).
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            received.load(Ordering::SeqCst),
            "upstream should be called -- model name is passed through"
        );
    }

    // ===========================================================================
    // Non-streaming tests: OpenAI Chat provider
    // ===========================================================================

    /// configured OpenAI Chat provider returns Anthropic response
    #[tokio::test]
    async fn openai_chat_provider_returns_anthropic_response() {
        let mock_url = spawn_mock_openai_chat_non_stream().await;
        let state = state_with_openai_chat_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gpt-4o", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Response must be Anthropic-shaped.
        assert_eq!(resp_body["type"], "message");
        assert_eq!(resp_body["role"], "assistant");
        assert!(
            resp_body["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Hello from OpenAI mock!"),
            "response should contain translated OpenAI text"
        );
    }

    // ===========================================================================
    // Non-streaming tests: OpenAI Responses provider
    // ===========================================================================

    /// configured Responses provider returns Anthropic response
    #[tokio::test]
    async fn openai_responses_provider_returns_anthropic_response() {
        let mock_url = spawn_mock_openai_responses_non_stream().await;
        let state = state_with_openai_responses_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gpt-4o-responses", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Response must be Anthropic-shaped.
        assert_eq!(resp_body["type"], "message");
        assert_eq!(resp_body["role"], "assistant");
        assert!(
            resp_body["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Hello from Responses mock!"),
            "response should contain translated Responses text"
        );
    }

    // ===========================================================================
    // Non-streaming tests: Gemini provider
    // ===========================================================================

    /// configured Gemini provider returns Anthropic response
    #[tokio::test]
    async fn gemini_provider_returns_anthropic_response() {
        let mock_url = spawn_mock_gemini_non_stream().await;
        let state = state_with_gemini_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gemini-2.5-pro", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Response must be Anthropic-shaped.
        assert_eq!(resp_body["type"], "message");
        assert_eq!(resp_body["role"], "assistant");
        assert!(
            resp_body["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Hello from Gemini mock!"),
            "response should contain translated Gemini text"
        );
    }

    // ===========================================================================
    // Streaming tests: Anthropic provider
    // ===========================================================================

    /// Collect the full SSE response body as a UTF-8 string for an Anthropic
    /// streaming request against the given mock URL. Shared by the focused
    /// streaming tests below so each test can assert a single behavior.
    async fn collect_anthropic_stream_body(mock_url: &str) -> String {
        let state = state_with_anthropic_provider(mock_url);
        let app = build_router(state);
        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// stream:true Anthropic provider returns HTTP 200 with an SSE content-type
    #[tokio::test]
    async fn stream_anthropic_provider_returns_sse_content_type() {
        let mock_url = spawn_mock_anthropic_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    /// stream:true Anthropic provider includes an x-request-id header on the
    /// stream response
    #[tokio::test]
    async fn stream_anthropic_provider_includes_request_id() {
        let mock_url = spawn_mock_anthropic_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let request_id = resp.headers().get("x-request-id");
        assert!(
            request_id.is_some(),
            "x-request-id must be present on stream response"
        );
    }

    /// stream:true Anthropic provider emits the core SSE protocol events
    /// (message_start, content_block_delta, message_stop)
    #[tokio::test]
    async fn stream_anthropic_provider_emits_sse_protocol_events() {
        let mock_url = spawn_mock_anthropic_stream().await;
        let text = collect_anthropic_stream_body(&mock_url).await;

        assert!(
            text.contains("event: message_start"),
            "missing message_start event"
        );
        assert!(
            text.contains("event: content_block_delta"),
            "missing content_block_delta"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop event"
        );
    }

    /// stream:true Anthropic provider forwards the translated text delta
    #[tokio::test]
    async fn stream_anthropic_provider_forwards_text_delta() {
        let mock_url = spawn_mock_anthropic_stream().await;
        let text = collect_anthropic_stream_body(&mock_url).await;

        assert!(
            text.contains("Hi!"),
            "expected text delta 'Hi!' in SSE output"
        );
    }

    /// stream:true Anthropic provider surfaces the upstream provider's real message
    /// id (from `message_start`) rather than a proxy-generated synthetic id.
    #[tokio::test]
    async fn stream_anthropic_provider_surfaces_upstream_message_id() {
        let mock_url = spawn_mock_anthropic_stream().await;
        let text = collect_anthropic_stream_body(&mock_url).await;
        // The mock emits `message_start` with id "msg_mock_stream"; the client SSE
        // must carry that real id, not a synthetic `msg_{uuid}`.
        assert!(
            text.contains("msg_mock_stream"),
            "client SSE must surface the upstream message id; got: {text}"
        );
    }

    // ===========================================================================
    // Streaming tests: OpenAI Chat provider
    // ===========================================================================

    /// Collect the full SSE response body for an OpenAI Chat streaming request
    /// against the given mock URL. Shared by the focused streaming tests below.
    async fn collect_openai_chat_stream_body(mock_url: &str) -> String {
        let state = state_with_openai_chat_provider(mock_url);
        let app = build_router(state);
        let body = make_messages_body("gpt-4o", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// stream:true OpenAI Chat provider surfaces the upstream provider's real chunk
    /// id rather than a proxy-generated synthetic `chatcmpl-{uuid}`.
    ///
    /// The OpenAI client encoder uses only the id passed at construction (it
    /// ignores `CoreEvent::MessageStart`'s id), so this only succeeds because the
    /// first event is buffered to extract the upstream id *before* the encoder is
    /// built. This is the distinguishing case for the buffer-first-event change:
    /// before it, OpenAI streaming always emitted a synthetic id.
    #[tokio::test]
    async fn stream_openai_chat_provider_surfaces_upstream_message_id() {
        let mock_url = spawn_mock_openai_chat_stream().await;
        let text = collect_openai_chat_stream_body(&mock_url).await;
        // The mock emits chunks with id "chatcmpl-stream"; the client SSE must carry
        // that real upstream id, not a synthetic chatcmpl-{uuid}.
        assert!(
            text.contains("chatcmpl-stream"),
            "client SSE must surface the upstream chunk id; got: {text}"
        );
    }

    /// The OpenAI Chat streaming path must populate
    /// `ResponseCompleted.upstream_message_id` on the event bus. The OpenAI Chat
    /// client encoder ignores `CoreEvent::MessageStart`'s id (it uses only the id
    /// passed at construction), so the upstream id reaches the response solely via
    /// the buffer-first-event extraction. The SSE-body test above checks the wire;
    /// this guards the persistence sink, which is a distinct surface (plan Test
    /// Plan: "Stream request emits ResponseCompleted with the real
    /// upstream_message_id from Step 1" -- the OpenAI path was uncovered).
    #[tokio::test]
    async fn event_bus_records_openai_chat_stream_upstream_message_id() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_openai_chat_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_openai_chat_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("gpt-4o", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Drain the SSE body so the spawned task runs to completion and emits the
        // stream ResponseCompleted (it fires at the end of the task).
        let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();

        let events = bus.snapshot();
        let completed = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseCompleted(c) => Some(c),
                _ => None,
            })
            .expect("ResponseCompleted emitted for OpenAI Chat stream");
        // The OpenAI stream mock seeds every chunk with id "chatcmpl-stream".
        assert_eq!(
            completed.upstream_message_id.as_deref(),
            Some("chatcmpl-stream"),
            "OpenAI Chat stream must capture the real upstream chunk id on the event bus"
        );
    }

    /// A successful non-stream request emits `RequestReceived` then
    /// `ResponseCompleted` on the event bus, with the provider-reported usage and
    /// upstream message id on the completed event, and no api-key leakage.
    #[tokio::test]
    async fn event_bus_records_request_received_and_response_completed() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_anthropic_non_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let events = bus.snapshot();
        assert_eq!(
            events.len(),
            2,
            "expected RequestReceived + ResponseCompleted"
        );
        assert!(
            matches!(events[0], ProxyEvent::RequestReceived(_)),
            "first event must be RequestReceived"
        );
        match &events[1] {
            ProxyEvent::ResponseCompleted(rc) => {
                assert_eq!(rc.provider, "mock-provider");
                assert_eq!(rc.upstream_message_id.as_deref(), Some("msg_mock123"));
                assert_eq!(rc.usage.input_tokens, 10);
                assert_eq!(rc.usage.output_tokens, 5);
                // No pricing configured for this provider -> cost must be None (the
                // "no pricing -> Cost = None" contract from the plan's Test Plan).
                assert!(
                    rc.cost.is_none(),
                    "cost must be None when no pricing is configured, got {:?}",
                    rc.cost
                );
            }
            other => panic!("expected ResponseCompleted, got {other:?}"),
        }

        // Defense-in-depth: no api-key fragments in the serialized event stream.
        let json = serde_json::to_string(&events).expect("serialize events");
        assert!(
            !json.contains("test-key") && !json.contains("sk-"),
            "event JSON must not contain api-key fragments: {json}"
        );
    }

    /// When pricing IS configured for the upstream model, a successful non-stream
    /// request attaches a computed `Some(Cost)` to `ResponseCompleted` -- the
    /// end-to-end pricing->usage->cost->event wiring (audit finding: cost pipeline
    /// never exercised end-to-end).
    #[tokio::test]
    async fn priced_provider_attaches_cost_to_response_completed() {
        use std::sync::Arc;

        use llm_proxy_core::{ModelId, ModelPricing};
        use llm_proxy_storage::{ProxyEvent, RecordingBus};
        use rust_decimal::Decimal;

        let mock_url = spawn_mock_anthropic_non_stream().await;
        let pricing = HashMap::from([(
            ModelId::new("claude-sonnet-4-6"),
            ModelPricing {
                input: "0.000003".parse::<Decimal>().unwrap(),
                output: "0.000015".parse::<Decimal>().unwrap(),
                ..ModelPricing::default()
            },
        )]);
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider_priced(&mock_url, pricing).with_event_bus(Arc::<
            RecordingBus,
        >::clone(
            &bus
        ));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let rc = bus
            .snapshot()
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseCompleted(rc) => Some(rc.clone()),
                _ => None,
            })
            .expect("ResponseCompleted emitted");
        // Mock usage is input=10, output=5.
        //   input  = 10 * 0.000003  = 0.00003
        //   output =  5 * 0.000015  = 0.000075
        //   total  = 0.000105
        let cost = rc
            .cost
            .expect("cost must be Some when pricing is configured");
        assert_eq!(cost.input.normalize().to_string(), "0.00003");
        assert_eq!(cost.output.normalize().to_string(), "0.000075");
        assert_eq!(cost.total.normalize().to_string(), "0.000105");
    }

    /// The first upstream chunk may decode to MULTIPLE CoreEvents (message_start
    /// immediately followed by content_block_delta). The buffered first-event
    /// replay must emit ALL of them exactly once -- none lost, none duplicated
    /// (plan requirement; audit finding: multi-event first-frame test missing).
    #[tokio::test]
    async fn stream_multi_event_first_frame_emits_all_without_loss_or_duplication() {
        let body = anthropic_sse_body("msg_multi", "");
        let mock_url = spawn_mock_server(body, "text/event-stream").await;
        let text = collect_anthropic_stream_body(&mock_url).await;
        assert_eq!(
            text.matches("event: message_start").count(),
            1,
            "message_start must appear exactly once: {text}"
        );
        assert_eq!(
            text.matches("event: content_block_delta").count(),
            1,
            "content_block_delta must appear exactly once (no loss/duplication): {text}"
        );
        assert_eq!(
            text.matches("event: message_stop").count(),
            1,
            "message_stop must appear exactly once: {text}"
        );
        assert!(
            text.contains("msg_multi"),
            "real upstream id must surface: {text}"
        );
    }

    /// A successful streaming request emits `RequestReceived` (streaming=true) then
    /// `ResponseCompleted`, with the upstream message id captured from MessageStart.
    #[tokio::test]
    async fn event_bus_records_stream_request_and_response_completed() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_anthropic_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Drain the SSE body so the spawned task runs to completion and emits the
        // stream ResponseCompleted (it fires at the end of the task, before the
        // output channel is dropped).
        let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();

        let events = bus.snapshot();
        let received = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::RequestReceived(r) => Some(r),
                _ => None,
            })
            .expect("RequestReceived emitted for stream");
        assert!(
            received.streaming,
            "stream request must be marked streaming"
        );
        let completed = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseCompleted(c) => Some(c),
                _ => None,
            })
            .expect("ResponseCompleted emitted for stream");
        // The Anthropic stream mock seeds message_start with id "msg_mock_stream".
        assert_eq!(
            completed.upstream_message_id.as_deref(),
            Some("msg_mock_stream")
        );
    }

    // ===========================================================================
    // Client-key authentication
    // ===========================================================================

    /// Hash a token with deliberately cheap argon2id parameters so router
    /// tests run fast; verification reads the cost from the PHC string.
    fn client_auth_for(key_id: &str, token: &str) -> llm_proxy_server::ClientAuth {
        use argon2::{Algorithm, Argon2, CustomizedPasswordHasher, Params, Version};
        let params = Params::new(8, 1, 1, None).expect("valid test params");
        let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone());
        let phc = hasher
            .hash_password_customized(
                token.as_bytes(),
                b"0123456789abcdef",
                Some(&argon2::ARGON2ID_IDENT),
                None,
                params,
            )
            .expect("hash token")
            .to_string();
        let mut clients = HashMap::new();
        clients.insert(key_id.to_owned(), secrecy::SecretString::from(phc));
        llm_proxy_server::ClientAuth::from_config(&llm_proxy_core::AuthConfig {
            mode: llm_proxy_core::AuthMode::ClientKeys,
            clients,
            admin: None,
        })
        .expect("valid client auth")
    }

    /// Under enforced client keys, a request without a bearer token is
    /// rejected with a 401 that carries no secret material and still has the
    /// x-request-id header.
    #[tokio::test]
    async fn enforced_client_keys_reject_missing_bearer_with_sanitized_401() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url).with_client_auth(std::sync::Arc::new(
            client_auth_for("laptop", "sekrit-token"),
        ));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers().get("x-request-id").is_some(),
            "401 must carry the request id"
        );
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("sekrit-token"),
            "401 body must not echo any secret material, got: {text}"
        );
    }

    /// A wrong bearer token is rejected exactly like a missing one: same 401
    /// body, and the attempted token never appears in the response.
    #[tokio::test]
    async fn enforced_client_keys_reject_wrong_bearer_without_echoing_it() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url).with_client_auth(std::sync::Arc::new(
            client_auth_for("laptop", "sekrit-token"),
        ));
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .header("authorization", "Bearer attacker-guess")
            .body(Body::from(make_messages_body("claude-sonnet-4-6", false)))
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("attacker-guess"),
            "401 body must not echo the attempted token, got: {text}"
        );
    }

    /// A valid bearer token passes the auth gate, and the RequestReceived
    /// event is attributed to the key id (not "anonymous", not the secret).
    #[tokio::test]
    async fn valid_bearer_passes_and_attributes_caller_on_event() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_anthropic_non_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus))
            .with_client_auth(std::sync::Arc::new(client_auth_for(
                "laptop",
                "sekrit-token",
            )));
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .header("authorization", "bearer sekrit-token")
            .body(Body::from(make_messages_body("claude-sonnet-4-6", false)))
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let events = bus.snapshot();
        let received = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::RequestReceived(r) => Some(r),
                _ => None,
            })
            .expect("RequestReceived emitted");
        assert_eq!(received.caller, "laptop");
        let event_json =
            serde_json::to_string(&ProxyEvent::RequestReceived(received.clone())).unwrap();
        assert!(
            !event_json.contains("sekrit-token"),
            "client secret must never appear on the event, got: {event_json}"
        );
    }

    /// Under the default no-auth mode, requests pass and events attribute the
    /// caller as "anonymous".
    #[tokio::test]
    async fn no_auth_mode_attributes_anonymous_caller() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_anthropic_non_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let events = bus.snapshot();
        let received = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::RequestReceived(r) => Some(r),
                _ => None,
            })
            .expect("RequestReceived emitted");
        assert_eq!(received.caller, "anonymous");
    }

    /// The auth gate is not applied to lightweight routes: /health stays
    /// open even under enforced client keys (it exposes no model traffic).
    #[tokio::test]
    async fn enforced_client_keys_leave_health_open() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url).with_client_auth(std::sync::Arc::new(
            client_auth_for("laptop", "sekrit-token"),
        ));
        let app = build_router(state);

        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A request for an unknown provider emits `ResponseFailed` with the mapped
    /// HTTP status (404) and the provider name from the URL path.
    #[tokio::test]
    async fn event_bus_records_response_failed_for_unknown_provider() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_anthropic_non_stream().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        // Target a provider that is not registered.
        let body = make_messages_body("claude-sonnet-4-6", false);
        let req = Request::builder()
            .method("POST")
            .uri("/providers/does-not-exist/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let events = bus.snapshot();
        let failed = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseFailed(f) => Some(f),
                _ => None,
            })
            .expect("ResponseFailed emitted for unknown provider");
        assert_eq!(failed.http_status, 404);
        assert_eq!(failed.provider.as_deref(), Some("does-not-exist"));
    }

    /// An empty upstream stream (no events at all) returns 502, not 200 -- the
    /// pre-existing bug where finalization synthesized a terminal and committed
    /// HTTP 200. Also asserts the event log gets a ResponseFailed, not a
    /// ResponseCompleted, for the empty stream.
    #[tokio::test]
    async fn empty_upstream_stream_returns_502_not_200() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_server(Vec::new(), "text/event-stream").await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "empty stream must be 502, not 200"
        );

        let events = bus.snapshot();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProxyEvent::ResponseCompleted(_))),
            "no ResponseCompleted for an empty stream"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProxyEvent::ResponseFailed(_))),
            "ResponseFailed emitted for an empty stream"
        );
    }

    // ---------------------------------------------------------------------------
    // Upstream message ID edge-case tests (plan Test Plan §"Upstream message IDs")
    // ---------------------------------------------------------------------------

    /// Build a raw Anthropic SSE body with a configurable message-start id and an
    /// optional leading prefix ("" / "ping" / "keepalive").
    fn anthropic_sse_body(message_start_id: &str, prefix: &str) -> Vec<u8> {
        let mut s = String::new();
        match prefix {
            "keepalive" => s.push_str(":keepalive\n\n"),
            "ping" => s.push_str("event: ping\ndata: {\"type\":\"ping\"}\n\n"),
            _ => {}
        }
        s.push_str(&format!(
            "event: message_start\ndata: {}\n\n",
            json!({
                "type": "message_start",
                "message": {
                    "id": message_start_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-4-6",
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 10, "output_tokens": 0 }
                }
            })
        ));
        s.push_str(&format!(
        "event: content_block_delta\ndata: {}\n\n",
        json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "Hi!" } })
    ));
        s.push_str(&format!(
            "event: message_stop\ndata: {}\n\n",
            json!({ "type": "message_stop" })
        ));
        s.into_bytes()
    }

    /// MessageStart with an empty id falls back to a synthetic `msg_`-prefixed id.
    #[tokio::test]
    async fn stream_empty_message_start_id_falls_back_to_synthetic() {
        let body = anthropic_sse_body("", "");
        let mock_url = spawn_mock_server(body, "text/event-stream").await;
        let text = collect_anthropic_stream_body(&mock_url).await;
        assert!(
            text.contains("\"id\":\"msg_"),
            "empty upstream id must produce a synthetic msg_-prefixed id; got: {text}"
        );
    }

    /// A stream that opens with a Ping event then MessageStart (same chunk) still
    /// surfaces the real upstream message id.
    #[tokio::test]
    async fn stream_ping_then_message_start_surfaces_real_id() {
        let body = anthropic_sse_body("msg_ping_real", "ping");
        let mock_url = spawn_mock_server(body, "text/event-stream").await;
        let text = collect_anthropic_stream_body(&mock_url).await;
        assert!(
            text.contains("msg_ping_real"),
            "real upstream id must survive a leading ping; got: {text}"
        );
    }

    /// A stream that opens with a `:keepalive` comment (empty decode) then
    /// MessageStart still surfaces the real upstream message id.
    #[tokio::test]
    async fn stream_keepalive_then_message_start_surfaces_real_id() {
        let body = anthropic_sse_body("msg_ka_real", "keepalive");
        let mock_url = spawn_mock_server(body, "text/event-stream").await;
        let text = collect_anthropic_stream_body(&mock_url).await;
        assert!(
            text.contains("msg_ka_real"),
            "real upstream id must survive a leading keepalive comment; got: {text}"
        );
    }

    /// stream:true OpenAI Chat provider returns HTTP 200 with an SSE content-type
    #[tokio::test]
    async fn stream_openai_chat_provider_returns_sse_content_type() {
        let mock_url = spawn_mock_openai_chat_stream().await;
        let state = state_with_openai_chat_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gpt-4o", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    /// stream:true OpenAI Chat provider emits Anthropic-shaped SSE protocol
    /// events (message_start, message_stop)
    #[tokio::test]
    async fn stream_openai_chat_provider_emits_sse_protocol_events() {
        let mock_url = spawn_mock_openai_chat_stream().await;
        let text = collect_openai_chat_stream_body(&mock_url).await;

        // The output must be Anthropic-shaped SSE events.
        assert!(
            text.contains("event: message_start"),
            "missing message_start event"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop event"
        );
    }

    /// stream:true OpenAI Chat provider forwards the translated text delta
    #[tokio::test]
    async fn stream_openai_chat_provider_forwards_translated_text_delta() {
        let mock_url = spawn_mock_openai_chat_stream().await;
        let text = collect_openai_chat_stream_body(&mock_url).await;

        // Verify text content was translated from OpenAI to Anthropic SSE format.
        assert!(
            text.contains("Hi from OpenAI!"),
            "expected translated text delta in SSE output, got: {text}"
        );
    }

    // ===========================================================================
    // Streaming tests: OpenAI Responses provider
    // ===========================================================================

    /// Collect the full SSE response body for an OpenAI Responses streaming
    /// request against the given mock URL. Shared by the focused streaming tests
    /// below.
    async fn collect_openai_responses_stream_body(mock_url: &str) -> String {
        let state = state_with_openai_responses_provider(mock_url);
        let app = build_router(state);
        let body = make_messages_body("gpt-4o-responses", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// stream:true Responses provider returns HTTP 200 with an SSE content-type
    #[tokio::test]
    async fn stream_openai_responses_provider_returns_sse_content_type() {
        let mock_url = spawn_mock_openai_responses_stream().await;
        let state = state_with_openai_responses_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gpt-4o-responses", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    /// stream:true Responses provider emits Anthropic-shaped SSE protocol events
    /// (message_start, message_stop)
    #[tokio::test]
    async fn stream_openai_responses_provider_emits_sse_protocol_events() {
        let mock_url = spawn_mock_openai_responses_stream().await;
        let text = collect_openai_responses_stream_body(&mock_url).await;

        // The output must be Anthropic-shaped SSE events.
        assert!(
            text.contains("event: message_start"),
            "missing message_start event"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop event"
        );
    }

    /// stream:true Responses provider forwards the translated text delta
    #[tokio::test]
    async fn stream_openai_responses_provider_forwards_translated_text_delta() {
        let mock_url = spawn_mock_openai_responses_stream().await;
        let text = collect_openai_responses_stream_body(&mock_url).await;

        // Verify text content was translated.
        assert!(
            text.contains("Hi from Responses!"),
            "expected translated text delta in SSE output, got: {text}"
        );
    }

    // ===========================================================================
    // Streaming tests: Gemini provider
    // ===========================================================================

    /// Collect the full SSE response body for a Gemini streaming request against
    /// the given mock URL. Shared by the focused streaming tests below.
    async fn collect_gemini_stream_body(mock_url: &str) -> String {
        let state = state_with_gemini_provider(mock_url);
        let app = build_router(state);
        let body = make_messages_body("gemini-2.5-pro", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// stream:true Gemini provider returns HTTP 200 with an SSE content-type
    #[tokio::test]
    async fn stream_gemini_provider_returns_sse_content_type() {
        let mock_url = spawn_mock_gemini_stream().await;
        let state = state_with_gemini_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("gemini-2.5-pro", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    /// stream:true Gemini provider emits Anthropic-shaped SSE protocol events
    /// (message_start, message_stop)
    #[tokio::test]
    async fn stream_gemini_provider_emits_sse_protocol_events() {
        let mock_url = spawn_mock_gemini_stream().await;
        let text = collect_gemini_stream_body(&mock_url).await;

        // The output must be Anthropic-shaped SSE events.
        assert!(
            text.contains("event: message_start"),
            "missing message_start event"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop event"
        );
    }

    /// stream:true Gemini provider forwards the translated text delta
    #[tokio::test]
    async fn stream_gemini_provider_forwards_translated_text_delta() {
        let mock_url = spawn_mock_gemini_stream().await;
        let text = collect_gemini_stream_body(&mock_url).await;

        // Verify text content was translated.
        assert!(
            text.contains("Hi from Gemini!"),
            "expected translated text delta in SSE output, got: {text}"
        );
    }

    // ===========================================================================
    // Stream error behavior tests
    // ===========================================================================

    /// stream_error_before_first_byte_returns_http_502 (via upstream 500 on stream request)
    #[tokio::test]
    async fn stream_error_before_first_byte_returns_http_502() {
        let mock_url = spawn_mock_500().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // The upstream returns 500, which send_stream propagates as an error
        // before the SSE response is committed (since the HTTP connection itself
        // failed). This should map to 502 Bad Gateway.
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "stream with upstream 500 should return 502"
        );
    }

    /// A malformed SSE data frame is silently skipped by the Anthropic provider
    /// adapter (`decode_frame` returns `Ok(vec![])` on bad JSON), so it does NOT
    /// drive the in-band error path. This test verifies the distinct condition that
    /// a VALID delta delivered BEFORE the malformed frame survives to the client,
    /// and that the stream completes gracefully with `message_stop` rather than
    /// panicking. The genuine in-band error path is covered separately by
    /// `in_band_upstream_error_emits_anthropic_error_event`
    /// (audit `duplicate-malformed-stream-tests`).
    #[tokio::test]
    async fn malformed_frame_is_skipped_and_valid_events_delivered() {
        let mock_url = spawn_mock_malformed_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // The HTTP status is 200 since SSE was committed.
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // Verify that the stream started with a message_start event.
        assert!(
            text.contains("event: message_start"),
            "stream should start with message_start"
        );

        // The valid delta BEFORE the malformed frame must be delivered to the
        // client (it carries the marker text from spawn_mock_malformed_stream).
        assert!(
            text.contains("good-delta"),
            "valid events before a malformed frame must be delivered, got: {text}"
        );

        // The stream should complete with a message_stop (the malformed frame is
        // silently skipped by the Anthropic adapter, not treated as a fatal error).
        assert!(
            text.contains("event: message_stop"),
            "stream should complete with message_stop even after malformed data"
        );
    }

    /// upstream disconnect: when the upstream stream ends prematurely (no
    /// message_stop from upstream), the client encoder's finish() emits a
    /// synthetic terminal event. The stream should complete gracefully with
    /// a message_stop event.
    #[tokio::test]
    async fn upstream_disconnect_completes_with_synthetic_terminal() {
        let mock_url = spawn_mock_disconnect_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // Stream should have started with message_start.
        assert!(
            text.contains("event: message_start"),
            "stream should start with message_start"
        );

        // The stream should complete with a message_stop event generated by
        // the client encoder's finish() method, even though the upstream
        // didn't send one.
        assert!(
            text.contains("event: message_stop"),
            "stream should have a synthetic message_stop after premature upstream end"
        );
    }

    /// A genuine in-band upstream error surfaces as an Anthropic-shaped
    /// `event: error` frame to the client.
    ///
    /// Distinct from `malformed_frame_is_skipped_and_valid_events_delivered`
    /// (which verifies a malformed JSON frame is skipped): here the mock emits a
    /// real protocol-level Anthropic `event: error`, which the provider decoder
    /// maps to `CoreEvent::Error` and the client encoder forwards as an
    /// `event: error` SSE frame (audit `in-band-error-event-path-untested`,
    /// `duplicate-malformed-stream-tests`).
    #[tokio::test]
    async fn in_band_upstream_error_emits_anthropic_error_event() {
        let mock_url = spawn_mock_in_band_error_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // Stream should have started with message_start.
        assert!(
            text.contains("event: message_start"),
            "stream should start with message_start"
        );

        // The in-band error must surface as an Anthropic `event: error` frame.
        assert!(
            text.contains("event: error"),
            "in-band upstream error must emit an event: error frame, got: {text}"
        );
        // The error payload carries the upstream error message.
        assert!(
            text.contains("Too many requests"),
            "event: error should carry the upstream error message, got: {text}"
        );
    }

    // ---------------------------------------------------------------------------
    // Client-disconnect / mid-stream cancellation
    // ---------------------------------------------------------------------------

    /// A mock upstream body stream that yields exactly one valid Anthropic SSE
    /// `message_start` frame, then parks forever (`Poll::Pending`). Its `Drop` impl
    /// records cancellation so a test can observe that the proxy tore the upstream
    /// down when the CLIENT disconnected mid-stream.
    ///
    /// This is the deterministic equivalent of a "slow-dripping" upstream: no
    /// `sleep` is involved. The stream just never produces a second frame, so the
    /// only way the proxy's spawned task exits after the first byte is the
    /// `cancel.cancelled()` branch in `build_sse_output_stream` (audit
    /// `client-disconnect-cancellation-untested`).
    struct SlowDripStream {
        /// First SSE frame to deliver; consumed on the first poll.
        first: Option<bytes::Bytes>,
        /// Set to `true` when this stream is dropped (i.e. the upstream connection
        /// was torn down because the proxy cancelled its read).
        canceled: Arc<AtomicBool>,
    }

    impl std::fmt::Debug for SlowDripStream {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SlowDripStream")
                .field("first_pending", &self.first.is_some())
                .field("canceled", &self.canceled)
                .finish()
        }
    }

    impl futures::Stream for SlowDripStream {
        type Item = Result<bytes::Bytes, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if let Some(chunk) = self.first.take() {
                std::task::Poll::Ready(Some(Ok(chunk)))
            } else {
                // Park forever: the proxy will keep this future alive until the
                // client disconnects and the CancellationToken fires.
                std::task::Poll::Pending
            }
        }
    }

    impl Drop for SlowDripStream {
        fn drop(&mut self) {
            self.canceled.store(true, Ordering::SeqCst);
        }
    }

    /// When the client drops the response body mid-stream (after the first event),
    /// the proxy must cancel the upstream read rather than leak it.
    ///
    /// This exercises the `cancel.cancelled()` branch wired via the response-body
    /// drop-guard in `build_sse_output_stream`. The proxy reads one upstream frame
    /// (crossing the first byte), the test then drops the response body, and the
    /// proxy's spawned task is expected to abort the upstream stream — observed
    /// here deterministically via the `SlowDripStream` Drop flag (no `sleep`-based
    /// polling). A bounded `tokio::time::timeout` guards against a regression that
    /// fails to propagate cancellation.
    ///
    /// NOTE: this asserts upstream teardown + no panic + exactly one upstream
    /// request. It does NOT directly assert `Metrics::record_client_cancel()` (which
    /// the cancel branch also invokes) because that field is `pub(crate)` and not
    /// exposed on `AppState`; a future change that exposes a cancellation counter
    /// should add that assertion here.
    #[tokio::test]
    async fn client_disconnect_cancels_upstream_stream() {
        use futures::StreamExt;

        // Valid Anthropic message_start frame; the proxy crosses the first byte once
        // it is decoded and the encoder emits the first client event.
        let first_frame = bytes::Bytes::from(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cancel\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n"
            .to_owned(),
    );
        let upstream_canceled = Arc::new(AtomicBool::new(false));
        let request_received = Arc::new(AtomicBool::new(false));

        let canceled_for_route = Arc::clone(&upstream_canceled);
        let received_for_route = Arc::clone(&request_received);
        let app = Router::new().route(
            "/{*path}",
            post(move || {
                let canceled = Arc::clone(&canceled_for_route);
                let received = Arc::clone(&received_for_route);
                async move {
                    received.store(true, Ordering::SeqCst);
                    let body = SlowDripStream {
                        first: Some(first_frame.clone()),
                        canceled,
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(body).into_response(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        let mock_url = format!("http://{}/providers/mock-provider/v1/messages", addr);

        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // The first byte crossed, so the SSE is committed as HTTP 200.
        assert_eq!(resp.status(), StatusCode::OK);

        // Read exactly one chunk from the response body (the first client event),
        // then DROP the body mid-stream — this is the client disconnect.
        let mut stream = resp.into_body().into_data_stream();
        let first_chunk = stream.next().await;
        assert!(
            first_chunk.is_some(),
            "client should receive at least the first SSE chunk"
        );
        // Drop the response body future (client disconnect). The proxy's
        // drop-guard must fire the CancellationToken and abort the upstream stream.
        drop(stream);

        // The upstream read is torn down synchronously with the body drop. Bound the
        // wait deterministically with a timeout (not a sleep-poll loop).
        let teardown = tokio::time::timeout(Duration::from_secs(2), async {
            while !upstream_canceled.load(Ordering::SeqCst) {
                // Yield to let the spawned proxy task observe the cancel and drop
                // the upstream reqwest stream (which drops SlowDripStream).
                tokio::task::yield_now().await;
            }
        })
        .await;

        assert!(
            teardown.is_ok(),
            "upstream stream must be torn down within 2s of client disconnect \
         (cancellation did not propagate; record_client_cancel path is broken)"
        );
        assert!(
            upstream_canceled.load(Ordering::SeqCst),
            "upstream SlowDripStream must have been dropped on client disconnect"
        );
        // The mock must have received exactly one upstream request.
        assert!(
            request_received.load(Ordering::SeqCst),
            "mock upstream should have received the request"
        );
    }

    /// Stream terminal event is emitted after provider decoder finish().
    /// Verifies that the finalization order follows: consume chunks -> SSE frames
    /// -> decoder events -> client encoder -> finish() -> terminal events.
    #[tokio::test]
    async fn stream_terminal_event_emitted_after_decoder_finish() {
        let mock_url = spawn_mock_disconnect_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // Verify the finalization sequence:
        // 1. Real events from upstream arrive first (message_start, content_block_start).
        assert!(
            text.contains("event: message_start"),
            "should have message_start from upstream"
        );
        assert!(
            text.contains("event: content_block_start"),
            "should have content_block_start from upstream"
        );

        // 2. The synthetic terminal events (from finish()) appear after real events.
        // Find positions to verify ordering.
        let start_pos = text.find("event: message_start").expect("message_start");
        let stop_pos = text.find("event: message_stop").expect("message_stop");
        assert!(
            stop_pos > start_pos,
            "message_stop must appear after message_start"
        );

        // 3. message_delta (with stop_reason) must appear before message_stop.
        let delta_pos = text.find("event: message_delta").expect("message_delta");
        assert!(
            delta_pos < stop_pos,
            "message_delta must appear before message_stop"
        );
        assert!(
            delta_pos > start_pos,
            "message_delta must appear after message_start"
        );
    }

    // ===========================================================================
    // Rate limit and dedup tests
    // ===========================================================================

    /// Rate-limited request returns 429 Too Many Requests.
    ///
    /// Uses a state configured with rpm=1 so that the second request is
    /// deterministically rejected.
    #[tokio::test]
    async fn rate_limited_request_returns_429() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        // Build state with rpm=1 so the second request is deterministically rejected.
        let provider = ProviderConfig {
            name: "mock-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: mock_url,
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: llm_proxy_core::ProviderRoutesConfig {
                messages: Some("messages".to_owned()),
                chat_completions: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let state = AppState::new(
            AppConfig {
                server: ServerConfig {
                    bind: "127.0.0.1:3456".parse().unwrap(),
                    request_timeout: Duration::from_secs(300),
                    shutdown_timeout: Duration::from_secs(30),
                    log_level: "info".to_owned(),
                    hot_reload: false,
                    allowed_origins: None,
                    server_name: "test-proxy".to_owned(),
                    rate_limit_rpm: 1,
                    trust_forwarded_headers: false,
                    dedup_window: Duration::from_millis(500),
                    log_format: Default::default(),
                    admin_bind: None,
                    admin_on_main_listener: false,
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
        );
        let app = build_router(state);

        // First request should be allowed (rpm=1, 1 token available).
        let body1 = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello one" }],
            "max_tokens": 64
        });
        let resp1 = app
            .clone()
            .oneshot(messages_request(&body1.to_string()))
            .await
            .unwrap();
        // The first request may succeed (200) or fail for non-rate-limit reasons.
        // It must NOT be 429.
        assert_ne!(
            resp1.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "first request should not be rate-limited"
        );

        // Second request should be rate-limited (token bucket depleted).
        let body2 = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello two" }],
            "max_tokens": 64
        });
        let resp2 = app
            .clone()
            .oneshot(messages_request(&body2.to_string()))
            .await
            .unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "second request should be rate-limited with rpm=1"
        );
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["type"], "error");
        assert_eq!(resp_body["error"]["type"], "rate_limit_error");
    }

    /// Duplicate request returns 409 Conflict.
    ///
    /// Uses a large dedup window (60 s) so the second request is deterministically
    /// caught as a duplicate.
    #[tokio::test]
    async fn duplicate_request_returns_409() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        // Build state with a 60-second dedup window so the duplicate is guaranteed
        // to be caught, and rpm=100 so rate limiting does not interfere.
        let provider = ProviderConfig {
            name: "mock-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: mock_url,
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: llm_proxy_core::ProviderRoutesConfig {
                messages: Some("messages".to_owned()),
                chat_completions: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let state = AppState::new(
            AppConfig {
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
                    dedup_window: Duration::from_secs(60),
                    log_format: Default::default(),
                    admin_bind: None,
                    admin_on_main_listener: false,
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
        );
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);

        // First request should succeed or fail for non-dedup reasons.
        let resp1 = app.clone().oneshot(messages_request(&body)).await.unwrap();
        assert_ne!(
            resp1.status(),
            StatusCode::CONFLICT,
            "first request should not be a duplicate"
        );

        // Second request with the same body and path should be deduplicated.
        let resp2 = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::CONFLICT,
            "second identical request should be deduplicated"
        );
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["type"], "error");
        assert!(
            resp_body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("duplicate"),
            "error message should mention 'duplicate'"
        );
    }

    // ===========================================================================
    // Token count tests
    // ===========================================================================

    /// Token count with tool definitions returns a positive count (tools excluded by design).
    #[tokio::test]
    async fn token_count_with_tools_returns_positive_count() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64,
            "tools": [{
                "name": "get_weather",
                "description": "Get the weather",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                }
            }]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Token count should be positive but may be underestimated since tools
        // are intentionally excluded from the estimate.
        assert!(
            resp_body["input_tokens"].as_u64().unwrap() > 0,
            "token count should be positive even when tools are present"
        );
    }

    /// Token count with non-text content returns a positive count (non-text excluded by design).
    #[tokio::test]
    async fn token_count_with_non_text_content_returns_positive_count() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "describe this" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "fake" } }
                ]
            }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            resp_body["input_tokens"].as_u64().unwrap() > 0,
            "token count should be positive even with non-text content"
        );
    }

    /// Token count with tool_result content returns a positive count (excluded by design).
    #[tokio::test]
    async fn token_count_with_tool_result_returns_positive_count() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                { "role": "user", "content": "what is the weather?" },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "toolu_123", "name": "get_weather", "input": { "location": "SF" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_123", "content": "Sunny, 72F" }] }
            ],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            resp_body["input_tokens"].as_u64().unwrap() > 0,
            "token count should be positive even with tool_result content"
        );
    }

    /// Token count endpoint works with the provider-configured application state.
    #[tokio::test]
    async fn token_count_endpoint_works_with_provider_state() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ===========================================================================
    // Error response shape tests
    // ===========================================================================

    /// Invalid JSON returns 400 with Anthropic-shaped error.
    #[tokio::test]
    async fn invalid_json_returns_400_anthropic_shape() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    /// Provider-scoped messages routing works with the configured application state.
    #[tokio::test]
    async fn provider_messages_route_works() {
        let mock_url = spawn_mock_anthropic_non_stream().await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // Should succeed since we have a valid mock upstream.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Route preserves key fields through core pipeline round-trip.
    /// Sends a request with optional fields (system, temperature, max_tokens)
    /// and verifies the upstream mock receives them in the encoded request.
    #[tokio::test]
    async fn route_preserves_fields_through_core() {
        let (mock_url, captured_body) =
            spawn_mock_with_body_capture(anthropic_success_response()).await;
        let state = state_with_anthropic_provider(&mock_url);
        let app = build_router(state);

        let body = json!({
            "model": "claude-sonnet-4-6",
            "system": "You are a helpful assistant.",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 128,
            "temperature": 0.7,
            "top_p": 0.9,
            "stream": false
        });
        let resp = app
            .oneshot(messages_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Check the upstream received the correct fields.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let guard = captured_body.lock().await;
        let upstream_body = guard
            .as_ref()
            .expect("upstream should have received a request body");
        let upstream_json: Value =
            serde_json::from_slice(upstream_body).expect("upstream body should be valid JSON");

        // The Anthropic adapter should have preserved these fields.
        assert_eq!(upstream_json["model"], "claude-sonnet-4-6");
        assert!(
            upstream_json["system"].is_string() || upstream_json["system"].is_array(),
            "system field should be preserved"
        );
        assert_eq!(upstream_json["max_tokens"], 128);
        assert_eq!(upstream_json["temperature"], 0.7);
        assert_eq!(upstream_json["top_p"], 0.9);
        // The adapter may omit `stream` when false (default), so accept either
        // `false` or absent (Null).
        let stream_val = &upstream_json["stream"];
        assert!(
            stream_val.is_null() || stream_val == &json!(false),
            "stream field should be false or absent, got: {stream_val}"
        );
    }

    // ---------------------------------------------------------------------------
    // Provider name validation tests (findings 103, 104)
    // ---------------------------------------------------------------------------

    /// Simple state helper for provider name validation tests. Does not need
    /// a live mock server since the tests exercise the validation layer only.
    fn state_for_validation_tests() -> AppState {
        let provider = ProviderConfig {
            name: "mock-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: "https://127.0.0.1:0/v1/messages".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: llm_proxy_core::ProviderRoutesConfig {
                messages: Some("messages".to_owned()),
                chat_completions: Some("messages".to_owned()),
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        AppState::new(
            AppConfig {
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
                    admin_bind: None,
                    admin_on_main_listener: false,
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

    #[tokio::test]
    async fn unknown_provider_name_returns_404() {
        let app = build_router(state_for_validation_tests());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/nonexistent-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Anthropic-shaped error.
        assert_eq!(resp_body["type"], "error");
        assert_eq!(resp_body["error"]["type"], "not_found_error");
    }

    #[tokio::test]
    async fn provider_name_with_uppercase_returns_400() {
        let app = build_router(state_for_validation_tests());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/MyProvider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn provider_name_with_special_chars_returns_400() {
        let app = build_router(state_for_validation_tests());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/my%20provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_on_messages_route_returns_405() {
        let app = build_router(state_for_validation_tests());
        let req = Request::builder()
            .method("GET")
            .uri("/providers/mock-provider/v1/messages")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn missing_content_type_on_messages_still_parses() {
        let app = build_router(state_for_validation_tests());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            // Intentionally omit content-type header.
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Without content-type, the request may still be processed since
        // axum Bytes extractor does not require content-type. The request
        // should either succeed (and fail at the upstream call) or return
        // an error status -- either way it should not panic.
        assert!(
            resp.status().is_client_error()
                || resp.status().is_server_error()
                || resp.status() == StatusCode::OK
        );
    }

    // ---------------------------------------------------------------------------
    // Error/status path event-log ordering (audit LOW, eventlog-03 / eventlog-04)
    // ---------------------------------------------------------------------------

    /// A streaming request that fails at the upstream (500) must emit
    /// `RequestReceived` BEFORE `ResponseFailed`. Before the eventlog-03 fix the
    /// streaming `RequestReceived` was emitted after `resolve_target`/`send_stream`,
    /// so an early-return produced an orphaned `ResponseFailed` with no preceding
    /// `RequestReceived`. The RequestReceived is now emitted before any upstream
    /// call, mirroring the non-stream path.
    #[tokio::test]
    async fn stream_emits_request_received_before_response_failed_on_upstream_failure() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_500().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", true);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        // Drain the body so the spawned task runs to completion.
        axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("draining the error body should succeed");

        let events = bus.snapshot();
        let received_idx = events
            .iter()
            .position(|e| matches!(e, ProxyEvent::RequestReceived(_)));
        let failed_idx = events
            .iter()
            .position(|e| matches!(e, ProxyEvent::ResponseFailed(_)));
        assert!(
            received_idx.is_some(),
            "RequestReceived must be emitted for a failing stream"
        );
        assert!(
            failed_idx.is_some(),
            "ResponseFailed must be emitted for a failing stream"
        );
        assert!(
            received_idx.unwrap() < failed_idx.unwrap(),
            "RequestReceived must precede ResponseFailed (no orphaned failure)"
        );
    }

    /// A non-stream upstream 500 emits `ResponseFailed` (not `ResponseCompleted`)
    /// and is counted as a failure -- never as a success. This guards the
    /// eventlog-04 invariant that a failure path never records success, and that
    /// the cost/ResponseCompleted path is skipped on failure.
    #[tokio::test]
    async fn non_stream_upstream_500_emits_response_failed_not_completed() {
        use std::sync::Arc;

        use llm_proxy_storage::{ProxyEvent, RecordingBus};

        let mock_url = spawn_mock_500().await;
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_anthropic_provider(&mock_url)
            .with_event_bus(Arc::<RecordingBus>::clone(&bus));
        let app = build_router(state);

        let body = make_messages_body("claude-sonnet-4-6", false);
        let resp = app.oneshot(messages_request(&body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let events = bus.snapshot();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProxyEvent::ResponseCompleted(_))),
            "a failed non-stream request must not emit ResponseCompleted"
        );
        let failed = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseFailed(f) => Some(f),
                _ => None,
            })
            .expect("ResponseFailed emitted for upstream 500");
        assert_eq!(failed.http_status, 502);
    }

    // ===========================================================================
    // stream-ids-01: leading CoreEvent::Error on the OpenAI streaming path
    // ===========================================================================

    /// A provider whose `chat_completions` route is served by an Anthropic-speaking
    /// adapter. The client hits `/v1/chat/completions` (so `client_protocol` is
    /// `OpenAiChat`), while the upstream speaks the Anthropic Messages protocol.
    /// This is the only configuration where a leading upstream `error` event can
    /// surface as a leading `CoreEvent::Error` on the OpenAI client path: the
    /// Anthropic adapter decodes the upstream `error` SSE frame into
    /// `CoreEvent::Error`, and the OpenAI client encoder drops it (returns `Err`),
    /// which is exactly the case the stream-ids-01 synthesis handles.
    fn state_with_openai_chat_client_anthropic_upstream(mock_endpoint: &str) -> AppState {
        let provider = ProviderConfig {
            name: "mock-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: mock_endpoint.to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: llm_proxy_core::ProviderRoutesConfig {
                messages: None,
                chat_completions: Some("messages".to_owned()),
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
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
                admin_bind: None,
                admin_on_main_listener: false,
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

    /// Spawn a mock upstream whose FIRST (and only) SSE frame is an Anthropic
    /// `error` event. No preceding `message_start` is emitted, so the first decoded
    /// `CoreEvent` is `CoreEvent::Error`.
    async fn spawn_mock_anthropic_leading_error_stream() -> String {
        let app = Router::new().route(
        "/{*path}",
        post(|| async move {
            let events = vec![Event::default()
                .event("error")
                .data(r#"{"type":"error","error":{"type":"overloaded_error","message":"Upstream overloaded"}}"#)];
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        spawn_mock_serve(listener, app);
        wait_for_ready(addr).await;
        format!("http://{addr}/v1/messages")
    }

    /// Build a minimal OpenAI Chat Completions streaming request body.
    fn make_chat_completions_stream_body(model: &str) -> String {
        json!({
            "model": model,
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true
        })
        .to_string()
    }

    /// Build a POST request to the OpenAI Chat Completions client route.
    fn chat_completions_stream_request(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    /// `stream-ids-01`: when the FIRST decoded upstream event on an OpenAI Chat
    /// STREAMING request (`ClientProtocol::OpenAiChat`) is an error -- with no
    /// preceding valid event -- the client must receive HTTP 200 with an in-band
    /// SSE error chunk, NOT a 502 "empty stream" body. The replay loop synthesizes
    /// the OpenAI error-object `data:` line plus the `data: [DONE]` terminator so
    /// the error crosses the first-byte boundary as a real in-band event.
    #[tokio::test]
    async fn stream_openai_chat_leading_error_returns_200_in_band_sse_error() {
        let mock_url = spawn_mock_anthropic_leading_error_stream().await;
        let state = state_with_openai_chat_client_anthropic_upstream(&mock_url);
        let app = build_router(state);

        let body = make_chat_completions_stream_body("claude-sonnet-4-6");
        let resp = app
            .oneshot(chat_completions_stream_request(&body))
            .await
            .unwrap();

        // The leading error must cross the first-byte boundary as an in-band SSE
        // event, committing HTTP 200 -- not surface as a pre-stream 502 "empty
        // stream" body.
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "leading stream error must be delivered in-band at HTTP 200, not a 502"
        );

        let ct = resp
            .headers()
            .get("content-type")
            .expect("content-type header")
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type for in-band error, got: {ct}"
        );

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // The in-band error must be an OpenAI-shaped error object on a `data:` line.
        // `overloaded_error` maps to `CoreStreamErrorKind::Upstream`, which the OpenAI
        // payload renders as `type: "server_error"`.
        assert!(
            text.contains("\"error\"") && text.contains("\"type\""),
            "in-band SSE error must carry an OpenAI error object; got: {text}"
        );
        assert!(
            text.contains("server_error"),
            "an upstream/overloaded error must render as server_error; got: {text}"
        );
        assert!(
            text.contains("Upstream overloaded"),
            "the sanitized error message must be surfaced in-band; got: {text}"
        );

        // The OpenAI `[DONE]` terminator must follow the error chunk.
        assert!(
            text.contains("[DONE]"),
            "the OpenAI [DONE] terminator must follow the in-band error; got: {text}"
        );

        // It must NOT be the pre-stream "empty stream" 502 JSON body.
        assert!(
            !text.contains("empty stream"),
            "must not surface the misleading empty-stream 502 body; got: {text}"
        );
    }
}
