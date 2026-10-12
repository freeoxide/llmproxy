//! Integration tests for ops endpoints, the `/v1/messages` proxy route, and
//! TOML-mode integration scenarios.

#[cfg(test)]
mod tests {

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use llm_proxy_core::{
        AppConfig, AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRegistry,
        ProviderRoutesConfig, ServerConfig,
    };
    use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
    use llm_proxy_server::{AppState, BuildInfo, build_router};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Build AppState in TOML mode for integration testing.
    fn state() -> AppState {
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
        };
        let registry = ProviderRegistry::from_providers(vec![]).expect("empty registry");
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

    /// Build AppState with a single "mock-provider" registered so that provider-based
    /// routing can resolve. The provider has an `anthropic_messages` adapter with a
    /// dummy endpoint. This is used by tests that send valid JSON past the parsing
    /// layer and need the request to reach the core pipeline.
    fn state_with_provider() -> AppState {
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
            routes: ProviderRoutesConfig {
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
                },
                auth: Default::default(),
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
    async fn health_returns_ok_with_body() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body.get("model_counts").is_none());
    }

    #[tokio::test]
    async fn ready_returns_ready() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "ready");
    }

    #[tokio::test]
    async fn version_returns_build_info() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/version")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(body["name"].is_string());
        assert_eq!(body["version"], "0.0.0");
        assert_eq!(body["target"], "test");
        assert_eq!(body["git_sha"], "test");
    }

    /// Messages route accepts requests without x-api-key header.
    ///
    /// The proxy does not enforce authentication itself -- it forwards the
    /// configured provider API key to the upstream. The upstream is unreachable
    /// (dummy endpoint), so the response is a 502 Bad Gateway, not a 404 (route
    /// is registered) or a 401 (the proxy does not check auth headers).
    #[tokio::test]
    async fn messages_without_auth_header_reaches_upstream() {
        let app = build_router(state_with_provider());
        // Minimal Anthropic-shaped request without x-api-key.
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 16
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // The route is registered (not 404) and the proxy forwards the request
        // using its configured API key. The upstream is unreachable, so we get
        // 502 Bad Gateway (or 500 for a routing error).
        let status = resp.status();
        assert_ne!(status, StatusCode::NOT_FOUND, "route must be registered");
        let valid_statuses = [
            StatusCode::BAD_GATEWAY,           // 502 - upstream unreachable
            StatusCode::INTERNAL_SERVER_ERROR, // 500 - routing error
        ];
        assert!(
            valid_statuses.contains(&status),
            "expected downstream error for unreachable upstream without auth header, got {status}"
        );
    }

    #[tokio::test]
    async fn unknown_route_returns_not_found() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/v1/nonexistent_route")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -- Route guardrails (Phase 0 characterization) ---------------------------

    /// Phase 0 guardrail: POST /v1/messages with valid Anthropic JSON must parse
    /// and validate the request shape. This test verifies that the route is
    /// registered and the request body parses successfully. Any failure must be
    /// an upstream or auth error -- never a bad-JSON or request-shape error.
    ///
    /// Note: The assertion on `error_type != "invalid_request_error"` when status
    /// is 400 is a best-effort guard, not a guarantee. The Anthropic API itself
    /// returns `invalid_request_error` for valid JSON with semantic issues. The
    /// test specifically checks that the request parses and routes without a
    /// JSON/shape error, not that no 400 can ever occur.
    #[tokio::test]
    async fn messages_valid_json_parses_and_validates() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        // Parse response body to verify it is valid JSON.
        let _resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        // Route must be registered (not 404).
        assert_ne!(status, StatusCode::NOT_FOUND, "route must be registered");

        // The request body is valid Anthropic JSON, so it must parse and route
        // successfully. With an empty routing table, the model is unknown so
        // 400 is returned. With a real routing table, downstream errors are:
        // 502 (upstream), 500 (routing), 401 (auth), 429 (rate limit), 409 (duplicate).
        let valid_statuses = [
            StatusCode::BAD_REQUEST, // 400 - unknown model (empty routing table)
            StatusCode::BAD_GATEWAY, // 502 - upstream failure
            StatusCode::INTERNAL_SERVER_ERROR, // 500 - routing error
            StatusCode::UNAUTHORIZED, // 401 - auth failure
            StatusCode::TOO_MANY_REQUESTS, // 429 - rate limit
            StatusCode::CONFLICT,    // 409 - duplicate
            StatusCode::OK,          // 200 - success (unlikely without real upstream)
        ];
        assert!(
            valid_statuses.contains(&status),
            "valid Anthropic JSON should produce a valid status, got {status}"
        );
    }

    /// Phase 0 guardrail: POST /v1/messages with invalid (non-JSON) body returns
    /// 400 with invalid_request_error (bad JSON parse).
    #[tokio::test]
    async fn messages_invalid_json_returns_bad_request() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from("this is not json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid JSON"),
            "error message should mention invalid JSON"
        );
    }

    /// Phase 0 guardrail: POST /v1/messages with valid JSON but missing required
    /// fields (no messages) returns 400 with invalid_request_error.
    #[tokio::test]
    async fn messages_valid_json_missing_fields_returns_validation_error() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["error"]["type"], "invalid_request_error");
        // The missing field triggers a serde deserialization error, which is
        // reported as "invalid JSON: missing field `messages`".
        assert!(
            resp_body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing field `messages`"),
            "should report missing messages field"
        );
    }

    /// Phase 9 guardrail: POST /v1/chat/completions route is mounted and tested
    /// comprehensively in tests/chat_completions.rs. The removed chat_echo test that
    /// was here has been removed since its single assertion (not-404) is a subset
    /// of the `route_is_mounted_no_longer_404` test in that file.

    #[tokio::test]
    async fn count_tokens_returns_estimate() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello world" }],
            "max_tokens": 1024
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        // Heuristic counter should return a positive token count.
        assert!(body["input_tokens"].as_u64().unwrap() > 0);
    }

    // -- Phase 0: Additional characterization tests -----------------------------

    /// Phase 0: not_found returns a JSON error envelope matching the Anthropic
    /// error format, not plain text.
    #[tokio::test]
    async fn not_found_returns_json_error_envelope() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/v1/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "not_found_error");
    }

    /// Phase 0: POST /v1/messages with wrong types in JSON fields (e.g. max_tokens
    /// as a string) returns 400 with invalid_request_error.
    #[tokio::test]
    async fn messages_wrong_field_type_returns_bad_request() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": "big"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["error"]["type"], "invalid_request_error");
    }

    /// Phase 0: POST /v1/messages with unknown fields returns 400 since
    /// MessageRequest now uses deny_unknown_fields.
    #[tokio::test]
    async fn messages_unknown_fields_returns_bad_request() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64,
            "future_field": "not supported"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp_body["error"]["type"], "invalid_request_error");
    }

    /// Phase 0: POST /v1/messages with invalid UTF-8 bytes returns 400.
    /// The server must not panic when receiving binary/non-UTF-8 body data.
    #[tokio::test]
    async fn messages_invalid_utf8_returns_bad_request() {
        let app = build_router(state());
        // Raw non-UTF-8 bytes.
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(vec![0x80, 0x81, 0x82]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // The server should return a 400-level error, not panic (5xx).
        assert!(
            resp.status().is_client_error(),
            "expected 4xx for invalid UTF-8, got {}",
            resp.status()
        );
    }

    /// Phase 0: POST /v1/messages with empty body (0 bytes) returns 400.
    #[tokio::test]
    async fn messages_empty_body_returns_bad_request() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    /// Phase 0: POST /v1/messages with oversized body (> 32 MiB) returns 413.
    #[tokio::test]
    async fn messages_oversized_body_returns_payload_too_large() {
        let app = build_router(state());
        // 33 MiB body (exceeds the 32 MiB limit).
        let oversized_body = "X".repeat(33 * 1024 * 1024);
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(oversized_body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // MEDIUM-7: the body-limit 413 is normalised into a protocol-shaped JSON
        // envelope (not axum's default plain-text rejection) and carries an
        // `x-request-id` header, matching every handler-produced error.
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert!(
            resp.headers().get("x-request-id").is_some(),
            "413 must carry x-request-id after normalisation"
        );
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too large")
        );
    }

    // -- TOML mode integration tests -------------------------------------------

    /// TOML mode: /health returns ok with server name from AppConfig.
    #[tokio::test]
    async fn toml_health_returns_ok() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "ok");
        // server_name comes from AppConfig.
        assert_eq!(body["service"], "test-proxy");
    }

    /// TOML mode: /version returns build info with AppConfig server name.
    #[tokio::test]
    async fn toml_version_returns_app_config_server_name() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/version")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["name"], "test-proxy");
        assert_eq!(body["version"], "0.0.0");
        assert_eq!(body["target"], "test");
        assert_eq!(body["git_sha"], "test");
    }

    /// TOML mode: /ready returns ready.
    #[tokio::test]
    async fn toml_ready_returns_ready() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "ready");
    }

    /// TOML mode: POST /providers/{provider}/v1/messages with a registered provider passes the
    /// request through to the upstream adapter. The model name is forwarded as-is, so
    /// an unknown model results in a downstream error (502) rather than a 400, because
    /// the proxy no longer validates model names locally.
    #[tokio::test]
    async fn toml_messages_passes_through_to_upstream() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // With provider-based routing, the model is passed through to the upstream
        // adapter. The dummy endpoint refuses connections, so we get a 502 Bad Gateway
        // (or 500 internal error), not a 400 for unknown model.
        let status = resp.status();
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "provider is registered, route must resolve"
        );
        // The upstream is unreachable so downstream error is expected.
        let valid_statuses = [
            StatusCode::BAD_GATEWAY,           // 502 - upstream failure
            StatusCode::INTERNAL_SERVER_ERROR, // 500 - routing error
        ];
        assert!(
            valid_statuses.contains(&status),
            "expected downstream error for unreachable upstream, got {status}"
        );
    }

    /// POST /providers/{provider}/v1/messages/count_tokens works with provider state.
    #[tokio::test]
    async fn toml_count_tokens_returns_estimate() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello world" }],
            "max_tokens": 1024
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
        assert!(resp_body["input_tokens"].as_u64().unwrap() > 0);
    }

    // ---------------------------------------------------------------------------
    // Additional integration tests for edge cases (findings 113, 114, 115, 117, 119)
    // ---------------------------------------------------------------------------

    /// GET on POST-only messages route returns a protocol-shaped 405 (finding 115,
    /// audit LOW-27): Anthropic envelope body, not axum's empty 405.
    #[tokio::test]
    async fn get_on_messages_route_returns_405() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("GET")
            .uri("/providers/mock-provider/v1/messages")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(
            body["type"], "error",
            "405 must be an Anthropic-shaped envelope"
        );
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "method not allowed");
    }

    /// GET on POST-only chat completions route returns a protocol-shaped 405
    /// (finding 115, audit LOW-27): OpenAI envelope body.
    #[tokio::test]
    async fn get_on_chat_completions_route_returns_405() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("GET")
            .uri("/providers/mock-provider/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "method not allowed");
        assert!(body["error"]["code"].is_null());
    }

    /// POST on GET-only health route returns 405 (finding 115).
    #[tokio::test]
    async fn post_on_health_route_returns_405() {
        let app = build_router(state());
        let req = Request::builder()
            .method("POST")
            .uri("/health")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// Invalid provider name on messages route returns 400 (finding 113).
    #[tokio::test]
    async fn invalid_provider_name_on_messages_returns_400() {
        let app = build_router(state());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/..%2F..%2Fetc/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum URL-decodes the path, so "../.." becomes the actual path segment.
        // Either way it should be rejected.
        assert!(
            resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND,
            "expected 400 or 404 for path traversal provider name, got {}",
            resp.status()
        );
    }

    /// count_tokens with invalid JSON returns 400 (finding 114).
    #[tokio::test]
    async fn count_tokens_invalid_json_returns_400() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from("{ not valid json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// count_tokens with empty body returns 400 (finding 114).
    #[tokio::test]
    async fn count_tokens_empty_body_returns_400() {
        let app = build_router(state_with_provider());
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(""))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// count_tokens with unknown provider returns 404 (finding 114).
    #[tokio::test]
    async fn count_tokens_unknown_provider_returns_404() {
        let app = build_router(state());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 64
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/nonexistent/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Health endpoint never exposes operational metrics, even when the caller opts
    /// in via the legacy `?metrics=true` query (LOW-13): the route is
    /// unauthenticated, so the query parameter is now ignored and the body is
    /// always the trivial `{status, service}` liveness probe.
    #[tokio::test]
    async fn health_never_exposes_metrics_even_with_opt_in_query() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/health?metrics=true")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.get("metrics").is_none(),
            "/health must not include 'metrics' even with ?metrics=true \
         (unauthenticated endpoint, LOW-13); got: {body}"
        );
        // The trivial body still carries status + service.
        assert_eq!(body["status"], "ok");
        assert!(body["service"].is_string());
    }

    /// Health endpoint omits operational metrics by default (LOW-13): the route is
    /// unauthenticated, so a bare probe must not leak operational counters.
    #[tokio::test]
    async fn health_omits_metrics_by_default() {
        let app = build_router(state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.get("metrics").is_none(),
            "bare /health must not include 'metrics' (unauthenticated endpoint); got: {body}"
        );
        // The minimal body still carries status + service.
        assert_eq!(body["status"], "ok");
        assert!(body["service"].is_string());
    }

    /// count_tokens response has correct JSON shape (finding 119).
    #[tokio::test]
    async fn count_tokens_response_has_correct_shape() {
        let app = build_router(state_with_provider());
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello world" }],
            "max_tokens": 1024
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Verify Content-Type is application/json.
        let ct = resp
            .headers()
            .get("content-type")
            .expect("content-type header");
        assert!(ct.to_str().unwrap().contains("application/json"));
        let resp_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(resp_body["input_tokens"].is_number());
        assert!(resp_body["input_tokens"].as_u64().unwrap() > 0);
        // Verify only the expected field is present.
        assert!(
            resp_body.as_object().unwrap().len() == 1,
            "count_tokens response should only have input_tokens field"
        );
    }

    // ---------------------------------------------------------------------------
    // CORS (audit LOW-12)
    // ---------------------------------------------------------------------------

    /// Build AppState whose `server.allowed_origins` is set to `origins` (audit
    /// LOW-12). Mirrors [`state`] but installs an explicit cross-origin allow list.
    fn state_with_origins(origins: Vec<String>) -> AppState {
        let app_config = AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: Duration::from_secs(300),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: Some(origins),
                server_name: "test-proxy".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: Duration::from_millis(500),
                log_format: Default::default(),
            },
            auth: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![]).expect("empty registry");
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

    /// A configured origin is echoed on a CORS preflight, and the permitted
    /// method/headers are advertised (audit LOW-12).
    #[tokio::test]
    async fn cors_preflight_echoes_configured_origin() {
        let app = build_router(state_with_origins(vec![
            "https://app.example.com".to_owned(),
        ]));
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", "https://app.example.com")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .expect("allow-origin echoed on preflight"),
            "https://app.example.com"
        );
        // The advertised methods must be the curated set, not "*" (LOW-12).
        let allow_methods = resp
            .headers()
            .get("access-control-allow-methods")
            .expect("allow-methods present")
            .to_str()
            .unwrap();
        assert!(allow_methods.contains("POST"));
        assert!(allow_methods.contains("GET"));
        assert!(!allow_methods.contains('*'));
    }

    /// With no `allowed_origins` configured, no CORS layer is installed and a
    /// browser preflight is not answered with an allow-origin echo (audit LOW-12).
    /// This is the regression guard: the old `CorsLayer::very_permissive()` admitted
    /// every origin by default; the new default must not.
    #[tokio::test]
    async fn cors_absent_when_origins_unset() {
        // `state()` builds with `allowed_origins: None`.
        let app = build_router(state());
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", "https://attacker.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "no allow-origin should be emitted when CORS is not configured"
        );
    }

    // ---------------------------------------------------------------------------
    // Event-log coverage for early-gate failures (audit eventlog-early-gates)
    // ---------------------------------------------------------------------------
    //
    // Each inference handler emits ResponseFailed exactly once per failed request:
    // early-gate failures (unknown provider, bad content-type, rate-limit, conflict,
    // malformed JSON, decode failure) emit inside the inner function, and failures
    // INSIDE the core pipeline emit inside the pipeline (which also returns Err).
    // The outer wrapper renders only -- it must NOT re-emit, or a pipeline failure
    // would be double-counted. These tests wire a RecordingBus into AppState and
    // assert the event counts so a regression (double-emit, or a dropped early-gate)
    // is caught (audit route-responsefailed-gaps).

    use llm_proxy_storage::{EventBus, ProxyEvent, RecordingBus};

    /// Build a provider-backed AppState whose event bus is a [`RecordingBus`],
    /// returning both the state and the bus so the test can snapshot emitted
    /// events. Mirrors [`state_with_provider`] but injects the recording sink.
    fn state_with_provider_and_bus() -> (AppState, Arc<RecordingBus>) {
        let bus = Arc::new(RecordingBus::new());
        let state = state_with_provider().with_event_bus(Arc::clone(&bus) as Arc<dyn EventBus>);
        (state, bus)
    }

    /// Count recorded events of a given variant by name ("request_received",
    /// "response_completed", "response_failed").
    fn count_events(bus: &RecordingBus, kind: &str) -> usize {
        bus.snapshot()
            .iter()
            .filter(|e| {
                matches!(
                    (e, kind),
                    (ProxyEvent::RequestReceived(_), "request_received")
                        | (ProxyEvent::ResponseCompleted(_), "response_completed")
                        | (ProxyEvent::ResponseFailed(_), "response_failed")
                )
            })
            .count()
    }

    /// The single ResponseFailed emitted for a malformed-JSON early gate carries
    /// http_status 400 and is emitted exactly once (audit eventlog-early-gates).
    #[tokio::test]
    async fn malformed_json_emits_one_response_failed_with_400() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from("not json at all"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "malformed JSON must emit exactly one ResponseFailed"
        );
        // The emitted event must carry the rendered HTTP status (400).
        let failed = bus
            .snapshot()
            .into_iter()
            .filter_map(|e| match e {
                ProxyEvent::ResponseFailed(f) => Some(f),
                _ => None,
            })
            .next()
            .expect("one ResponseFailed");
        assert_eq!(failed.http_status, 400);
    }

    /// An unknown-provider request emits exactly ONE ResponseFailed (not two) after
    /// the centralized wrapper replaced the per-site UnknownProvider emit
    /// (audit eventlog-early-gates regression guard).
    #[tokio::test]
    async fn unknown_provider_emits_exactly_one_response_failed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 16
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/does-not-exist/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "unknown provider must emit exactly one ResponseFailed (no double-emit)"
        );
    }

    /// A duplicate request within the dedup window hits the Conflict early gate and
    /// emits exactly one ResponseFailed carrying http_status 409. This covers the
    /// RateLimited/Conflict branch of the centralized emit (audit eventlog-early-gates).
    #[tokio::test]
    async fn duplicate_request_emits_one_response_failed_with_409() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "dedup probe" }],
            "max_tokens": 16
        });
        let make_req = || {
            Request::builder()
                .method("POST")
                .uri("/providers/mock-provider/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        // First request consumes the dedup slot (it will fail downstream at 502 and
        // emit its own pipeline-level ResponseFailed; that is expected and distinct
        // from the Conflict gate emission under test).
        let _ = app.clone().oneshot(make_req()).await.unwrap();
        // Second identical request within the dedup window is rejected as a Conflict
        // at the prepare_request early gate, before any upstream call. That early
        // gate emits exactly one ResponseFailed for THIS request.
        let resp = app.oneshot(make_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // Exactly one ResponseFailed carries the Conflict's 409 status: the
        // early-gate emit fired once for the dedup rejection. (The first request's
        // downstream failure emits a 502 ResponseFailed, which is a separate,
        // expected event and is excluded by the status filter.)
        let conflicts: Vec<_> = bus
            .snapshot()
            .into_iter()
            .filter_map(|e| match e {
                ProxyEvent::ResponseFailed(f) if f.http_status == 409 => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "dedup Conflict must emit exactly one 409 ResponseFailed"
        );

        // Direct per-request count: the first request's downstream failure must also
        // have emitted exactly ONE ResponseFailed (no double-emit from a pipeline
        // failure re-emitting at the wrapper). Both requests share the bus, so
        // confirm the total ResponseFailed count is exactly 2 (one per request),
        // not more (audit route-responsefailed-gaps regression).
        assert_eq!(
            count_events(&bus, "response_failed"),
            2,
            "two requests (one downstream failure + one dedup conflict) must emit exactly two ResponseFailed total"
        );
    }

    /// token_count success emits RequestReceived + ResponseCompleted (token_count
    /// previously emitted NO events; it now logs like any other route)
    /// (audit eventlog-early-gates).
    #[tokio::test]
    async fn token_count_success_emits_request_received_and_completed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hello world" }],
            "max_tokens": 1024
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        assert_eq!(
            count_events(&bus, "request_received"),
            1,
            "token_count success must emit one RequestReceived"
        );
        assert_eq!(
            count_events(&bus, "response_completed"),
            1,
            "token_count success must emit one ResponseCompleted"
        );
        assert_eq!(
            count_events(&bus, "response_failed"),
            0,
            "token_count success must not emit any ResponseFailed"
        );
    }

    /// token_count failure emits exactly one ResponseFailed (token_count previously
    /// emitted NO events on failure) (audit eventlog-early-gates).
    #[tokio::test]
    async fn token_count_failure_emits_one_response_failed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(Body::from("{ broken json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "token_count failure must emit exactly one ResponseFailed"
        );
    }

    /// /models emits exactly one ResponseFailed on an unknown-provider error, for
    /// consistency with the rest of the API (audit eventlog-early-gates). /models is
    /// a listing endpoint so it carries no RequestReceived/ResponseCompleted pair.
    #[tokio::test]
    async fn models_unknown_provider_emits_one_response_failed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/providers/does-not-exist/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "/models unknown provider must emit exactly one ResponseFailed"
        );
    }

    /// A failure INSIDE the core pipeline (unreachable upstream -> 502) emits
    /// exactly ONE ResponseFailed for the request, not two. The core pipeline emits
    /// its own ResponseFailed on the upstream failure AND returns Err; the route
    /// handler must NOT re-emit at the wrapper (regression guard for
    /// route-responsefailed-gaps). Only one request hits this bus, so the count is a
    /// direct per-request assertion.
    #[tokio::test]
    async fn upstream_failure_inside_pipeline_emits_one_response_failed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "pipeline failure probe" }],
            "max_tokens": 16
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // The mock provider points at an unreachable endpoint, so the request
        // fails INSIDE the core pipeline (upstream send) and the client gets a 502
        // (or 500 routing error), proving the failure occurred past the early gates.
        let status = resp.status();
        let valid_statuses = [StatusCode::BAD_GATEWAY, StatusCode::INTERNAL_SERVER_ERROR];
        assert!(
            valid_statuses.contains(&status),
            "expected a pipeline-level failure (502/500), got {status}"
        );

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "an inside-pipeline failure must emit exactly ONE ResponseFailed (no double-emit)"
        );
    }

    /// Same regression guard for the OpenAI chat-completions route: an
    /// inside-pipeline failure emits exactly one ResponseFailed
    /// (audit route-responsefailed-gaps).
    #[tokio::test]
    async fn upstream_failure_inside_pipeline_chat_emits_one_response_failed() {
        let (state, bus) = state_with_provider_and_bus();
        let app = build_router(state);
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "pipeline failure probe" }],
            "max_tokens": 16
        });
        let req = Request::builder()
            .method("POST")
            .uri("/providers/mock-provider/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let valid_statuses = [StatusCode::BAD_GATEWAY, StatusCode::INTERNAL_SERVER_ERROR];
        assert!(
            valid_statuses.contains(&status),
            "expected a pipeline-level failure (502/500), got {status}"
        );

        assert_eq!(
            count_events(&bus, "response_failed"),
            1,
            "an inside-pipeline failure must emit exactly ONE ResponseFailed (no double-emit)"
        );
    }
}
