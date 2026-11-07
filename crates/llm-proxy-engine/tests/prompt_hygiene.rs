//! End-to-end prompt-cache hygiene (plan 06 step 7, backlog 15): the
//! `[provider.prompt_cache]` opt-in reaches the provider edge through the
//! engine, and a same-format request's volatile bytes stop reaching the
//! upstream — so the provider's prompt cache keys stay stable across turns.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use llm_proxy_adapters::client_wire::{ClientWireFormat, ClientWireRequest};
use llm_proxy_core::{
    AppConfig, AuthStyle, PromptCacheConfig, ProviderAdapterConfig, ProviderConfig,
    ProviderRegistry, ProviderRouteKind, ProviderRoutesConfig, ServerConfig,
};
use llm_proxy_engine::{ClientDialect, Engine, EngineBuilder, EngineRequest, InboundRequest};
use llm_proxy_protocol::core::CoreRequest;
use llm_proxy_storage::{EventBus, NoopBus};
use tokio::sync::Mutex;

fn test_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("bind addr"),
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            log_level: "info".to_owned(),
            hot_reload: false,
            allowed_origins: None,
            server_name: "engine-test".to_owned(),
            rate_limit_rpm: 1000,
            trust_forwarded_headers: false,
            log_format: Default::default(),
            admin_bind: None,
            admin_on_main_listener: false,
            endpoint_families: Default::default(),
            upstream_timeouts: Default::default(),
            bare_v1: Default::default(),
            preflight: Default::default(),
            cache: Default::default(),
            idempotency: Default::default(),
            metrics: Default::default(),
        },
        auth: Default::default(),
        storage: None,
    }
}

fn provider(endpoint: String, prompt_cache: Option<PromptCacheConfig>) -> ProviderConfig {
    let mut adapters = std::collections::HashMap::new();
    adapters.insert(
        "messages".to_owned(),
        ProviderAdapterConfig {
            protocol: "anthropic_messages".to_owned(),
            endpoint,
            headers: Arc::new(std::collections::HashMap::new()),
            forward_inbound_headers: None,
            lossy: false,
        },
    );
    ProviderConfig {
        name: "fake".to_owned(),
        api_key: secrecy::SecretString::from("upstream-key"),
        auth_style: AuthStyle::XApiKey,
        passthrough_auth: false,
        api_keys: None,
        retry: None,
        egress: None,
        adapters,
        routes: ProviderRoutesConfig {
            messages: Some("messages".to_owned()),
            chat_completions: None,
        },
        model_aliases: std::collections::HashMap::new(),
        discovery: None,
        catalog: None,
        pricing: Default::default(),
        concurrency: None,
        pricing_sync: None,
        prompt_cache,
    }
}

/// A same-format (Anthropic dialect) client body whose only difference per
/// turn is the volatile `metadata.user_id` correlation token.
fn turn_body(user_id: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "metadata": {"user_id": user_id},
        "system": [
            {"type": "text", "text": "You are a coding agent."}
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "refactor this module"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "on it"}]},
            {"role": "user", "content": [{"type": "text", "text": "the tests next"}]}
        ]
    })
}

fn engine_request(body: serde_json::Value, request_id: &str) -> EngineRequest {
    let core: CoreRequest = llm_proxy_protocol::client::anthropic::decode_request(
        serde_json::from_value(body.clone()).expect("wire parses"),
    )
    .expect("core decodes");
    EngineRequest::new(
        "fake",
        ProviderRouteKind::Messages,
        ClientDialect::Anthropic,
        core,
        InboundRequest {
            auth: None,
            headers: Default::default(),
            wire: ClientWireRequest::new(ClientWireFormat::AnthropicMessages, body),
        },
    )
    .with_request_id(request_id)
    .with_caller("caller-a")
    .with_body_hash(request_id.to_owned())
}

async fn spawn_capturing_upstream() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let app = Router::new().route(
        "/{*path}",
        axum::routing::post(move |body: String| {
            let sink = Arc::clone(&sink);
            async move {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                sink.lock().await.push(value);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"id":"msg_h","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-sonnet-4-6","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":4}}"#,
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::trace!(%error, "fake upstream stopped");
        }
    });
    (format!("http://{addr}/v1/messages"), captured)
}

async fn build_engine(endpoint: String, prompt_cache: Option<PromptCacheConfig>) -> Engine {
    let registry =
        ProviderRegistry::from_providers(vec![provider(endpoint, prompt_cache)]).expect("registry");
    EngineBuilder::new(test_config())
        .providers(Arc::new(registry))
        .event_bus(Arc::new(NoopBus) as Arc<dyn EventBus>)
        .build()
        .expect("engine builds")
}

#[tokio::test]
async fn without_the_opt_in_volatile_bytes_reach_the_upstream() {
    let (endpoint, captured) = spawn_capturing_upstream().await;
    let engine = build_engine(endpoint, None).await;

    engine
        .execute(engine_request(turn_body("turn-1-token"), "hash-1"))
        .await
        .expect("first turn");
    let bodies = captured.lock().await;
    let upstream = bodies.last().expect("one upstream body");
    assert_eq!(
        upstream["metadata"]["user_id"],
        serde_json::json!("turn-1-token"),
        "same-format passthrough forwards the client body — volatile bytes included — \
         until the operator opts into hygiene"
    );
}

#[tokio::test]
async fn opted_in_hygiene_keeps_upstream_prefixes_stable_across_turns() {
    let (endpoint, captured) = spawn_capturing_upstream().await;
    let engine = build_engine(
        endpoint,
        Some(PromptCacheConfig {
            strip_volatile: vec!["/metadata/user_id".to_owned()],
            inject_markers: true,
        }),
    )
    .await;

    for (turn, token) in [("turn-1-token", "hash-1"), ("turn-2-token", "hash-2")] {
        engine
            .execute(engine_request(turn_body(token), turn))
            .await
            .expect("turn runs");
    }

    let bodies = captured.lock().await;
    assert_eq!(bodies.len(), 2);
    for body in bodies.iter() {
        assert!(
            body.get("metadata")
                .and_then(|m| m.get("user_id"))
                .is_none(),
            "the volatile correlation token must never reach the upstream: {body}"
        );
    }
    // The provider's prompt-cache key: the same stable prefix on both turns
    // — system and messages byte-identical, and the injected markers in the
    // same places.
    assert_eq!(
        bodies[0]["system"], bodies[1]["system"],
        "the prefix must be identical across turns"
    );
    assert_eq!(bodies[0]["messages"], bodies[1]["messages"]);
    assert!(
        bodies[0]["system"][0]["cache_control"].is_object()
            && bodies[0]["messages"][1]["content"][0]["cache_control"].is_object(),
        "marker injection marks the stable prefix: {}",
        bodies[0]
    );
    assert!(
        bodies[0]["messages"][2]["content"][0]["cache_control"].is_null(),
        "the newest turn is never marked"
    );
}
