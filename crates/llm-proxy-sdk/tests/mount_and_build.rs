//! Plan 03 facade tests: an axum app mounts the proxy router next to its own
//! routes, and the whole stack assembles from code (no TOML file).

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use llm_proxy_sdk::prelude::*;
use tower::ServiceExt;

fn app_config() -> core::AppConfig {
    core::AppConfig {
        server: core::ServerConfig {
            bind: "127.0.0.1:0".parse().expect("bind"),
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            log_level: "info".to_owned(),
            hot_reload: false,
            allowed_origins: None,
            server_name: "sdk-test".to_owned(),
            rate_limit_rpm: 10_000,
            trust_forwarded_headers: false,
            cache: Default::default(),
            idempotency: Default::default(),
            metrics: Default::default(),
            log_format: Default::default(),
            admin_bind: None,
            admin_on_main_listener: false,
            endpoint_families: HashMap::new(),
            upstream_timeouts: Default::default(),
            bare_v1: Default::default(),
            preflight: Default::default(),
            semantic_cache: Default::default(),
        },
        auth: Default::default(),
        storage: None,
        replica: None,
        mcp: None,
    }
}

fn state() -> AppState {
    let registry = core::ProviderRegistry::from_providers(vec![]).expect("empty registry");
    AppState::new(
        app_config(),
        registry,
        adapters::ProviderAdapterRegistry::builtin(),
        transport::ProxyClient::new(),
        BuildInfo {
            name: "sdk-test",
            version: "0.0.0",
            target: "test",
            git_sha: "test",
        },
    )
}

#[tokio::test]
async fn an_axum_app_mounts_the_proxy_router_next_to_its_own_routes() {
    // A host application with its own routes merges the proxy router in.
    let app = Router::new()
        .route("/app/health", get(|| async { "host-app-ok" }))
        .merge(build_router(state()));

    let host = app
        .clone()
        .oneshot(
            Request::get("/app/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("host route serves");
    assert_eq!(host.status(), StatusCode::OK);
    let host_body = axum::body::to_bytes(host.into_body(), 1024)
        .await
        .expect("host body");
    assert_eq!(&host_body[..], b"host-app-ok");

    let proxy = app
        .clone()
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("proxy route serves");
    assert_eq!(proxy.status(), StatusCode::OK);
    let proxy_body = axum::body::to_bytes(proxy.into_body(), 1024)
        .await
        .expect("proxy body");
    assert!(
        proxy_body.starts_with(b"{"),
        "the proxy /health answers its own JSON, got: {proxy_body:?}"
    );

    // And the proxy's own semantics still hold inside the host app.
    let unknown = app
        .clone()
        .oneshot(
            Request::post("/providers/does-not-exist/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await
        .expect("unknown provider handled");
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_stack_assembles_from_code_without_toml() {
    let engine = ProxyBuilder::new()
        .app_config(app_config())
        .engine()
        .expect("engine builds from code");

    // The pipeline is reachable without any HTTP listener: an unknown
    // provider resolves through the engine directly.
    let request = EngineRequest::new(
        "missing",
        core::ProviderRouteKind::Messages,
        ClientDialect::Anthropic,
        {
            let messages = vec![protocol::core::CoreMessage {
                role: protocol::core::CoreRole::User,
                content: vec![protocol::core::CoreContent::Text {
                    text: "x".to_owned(),
                    cache: None,
                }],
            }];
            protocol::core::CoreRequest {
                model: protocol::core::ModelRef {
                    requested: "m".to_owned(),
                    upstream: None,
                },
                system: Vec::new(),
                messages,
                tools: Vec::new(),
                tool_choice: None,
                sampling: protocol::core::SamplingOptions::default(),
                stream: false,
                metadata: Default::default(),
                provider_hints: Default::default(),
            }
        },
        engine::InboundRequest {
            auth: None,
            headers: Default::default(),
            wire: ClientWireRequest::new(
                ClientWireFormat::AnthropicMessages,
                serde_json::json!({}),
            ),
        },
    );
    let error = engine.execute(request).await.expect_err("no such provider");
    assert!(matches!(error, EngineError::UnknownProvider(_)));
}

const SDK_ECHO_PROTOCOL: &str = "sdk_echo_v1";

/// A minimal embedder-supplied codec, registered through [`ProxyBuilder`]
/// the way any custom protocol would be.
#[derive(Debug)]
struct SdkEchoAdapter;

/// The echo codec never streams; the trait still requires a decoder.
#[derive(Debug)]
struct EmptyDecoder;

impl adapters::ProviderStreamDecoder for EmptyDecoder {
    fn decode_frame(
        &mut self,
        _frame: &adapters::SseFrame,
    ) -> Result<Vec<protocol::core::CoreEvent>, adapters::ProviderError> {
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<protocol::core::CoreEvent>, adapters::ProviderError> {
        Ok(Vec::new())
    }

    fn saw_terminal(&self) -> bool {
        false
    }
}

impl adapters::ProviderAdapter for SdkEchoAdapter {
    fn protocol(&self) -> adapters::ProtocolName {
        adapters::ProtocolName::new(SDK_ECHO_PROTOCOL)
    }

    fn encode_request(
        &self,
        _core: &protocol::core::CoreRequest,
        target: &adapters::ProviderAdapterTarget,
        _client: &ClientWireRequest,
    ) -> Result<adapters::request::ProxyRequest, adapters::ProviderError> {
        Ok(adapters::request::ProxyRequest {
            url: target.endpoint.clone(),
            auth: adapters::request::AuthHeaders {
                style: target.auth_style,
                api_key: target.api_key.clone(),
            },
            body: Vec::new(),
            stream: false,
            extra_headers: HashMap::new(),
            forwarded_headers: HashMap::new(),
        })
    }

    fn decode_response(
        &self,
        _bytes: &[u8],
        target: &adapters::ProviderAdapterTarget,
    ) -> Result<protocol::core::CoreResponse, adapters::ProviderError> {
        Ok(protocol::core::CoreResponse {
            id: None,
            model: protocol::core::ModelRef {
                requested: target.requested_model.clone(),
                upstream: Some(target.upstream_model.clone()),
            },
            content: Vec::new(),
            stop_reason: protocol::core::StopReason::EndTurn,
            stop_sequence: None,
            usage: protocol::core::Usage::default(),
            provider_meta: serde_json::Map::new(),
            cost: None,
        })
    }

    fn new_stream_decoder(
        &self,
        _target: &adapters::ProviderAdapterTarget,
    ) -> Box<dyn adapters::ProviderStreamDecoder + Send> {
        Box::new(EmptyDecoder)
    }
}

#[tokio::test]
async fn a_custom_protocol_registers_through_the_builder_alongside_the_builtins() {
    let builtin = ProxyBuilder::new()
        .app_config(app_config())
        .engine()
        .expect("engine builds from the built-ins alone")
        .adapters()
        .protocol_names();

    let with_custom = ProxyBuilder::new()
        .adapter(Arc::new(SdkEchoAdapter))
        .expect("custom adapter registers")
        .app_config(app_config())
        .engine()
        .expect("engine builds with the custom adapter")
        .adapters()
        .protocol_names();

    assert!(
        with_custom.iter().any(|name| name == SDK_ECHO_PROTOCOL),
        "the custom protocol must reach the built engine, got {with_custom:?}"
    );
    assert_eq!(
        with_custom.len(),
        builtin.len() + 1,
        "registration adds exactly the custom protocol, keeping {builtin:?}"
    );
}
