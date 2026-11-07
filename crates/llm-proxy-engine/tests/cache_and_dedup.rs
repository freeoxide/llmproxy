//! Plan 06 integration tests: per-caller single-flight, the opt-in
//! `Idempotency-Key` store, and the opt-in exact-match response cache,
//! against a loopback fake upstream.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use futures::StreamExt;
use llm_proxy_adapters::client_wire::{ClientWireFormat, ClientWireRequest};
use llm_proxy_core::{
    AppConfig, AuthStyle, IdempotencyConfig, ProviderAdapterConfig, ProviderConfig,
    ProviderRegistry, ProviderRouteKind, ProviderRoutesConfig, ServerConfig,
};
use llm_proxy_engine::{
    ClientDialect, Engine, EngineBuilder, EngineError, EngineRequest, InboundRequest, MemoryCache,
};
use llm_proxy_protocol::core::{
    CoreContent, CoreEvent, CoreMessage, CoreRequest, CoreRole, ModelRef, SamplingOptions,
};
use llm_proxy_storage::{EventBus, ProxyEvent, RecordingBus};
use sha2::{Digest as _, Sha256};

/// A slow fake upstream: counts calls and answers a fixed non-streaming
/// response after `delay`, so identical requests overlap in flight.
struct CountingUpstream {
    calls: Arc<AtomicUsize>,
    delay: Duration,
    status: u16,
}

impl CountingUpstream {
    fn new(delay: Duration) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            delay,
            status: 200,
        }
    }

    fn failing() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            delay: Duration::ZERO,
            status: 500,
        }
    }

    fn router(&self) -> Router {
        let calls = Arc::clone(&self.calls);
        let delay = self.delay;
        let status = self.status;
        Router::new().route(
            "/{*path}",
            axum::routing::post(move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                if status == 200 {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hello from upstream"}],"model":"claude-sonnet-4-6","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":4}}"#,
                    )
                        .into_response()
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, "upstream broke")
                        .into_response()
                }
            }),
        )
    }
}

async fn spawn_upstream(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let addr = listener.local_addr().expect("fake upstream addr");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::trace!(%error, "fake upstream stopped");
        }
    });
    format!("http://{addr}/v1/messages")
}

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
        },
        auth: Default::default(),
        storage: None,
    }
}

fn provider(endpoint: String) -> ProviderConfig {
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
    let routes = ProviderRoutesConfig {
        messages: Some("messages".to_owned()),
        chat_completions: None,
    };
    ProviderConfig {
        name: "fake".to_owned(),
        api_key: secrecy::SecretString::from("upstream-key"),
        auth_style: AuthStyle::XApiKey,
        passthrough_auth: false,
        api_keys: None,
        retry: None,
        egress: None,
        adapters,
        routes,
        model_aliases: std::collections::HashMap::new(),
        discovery: None,
        catalog: None,
        pricing: Default::default(),
        concurrency: None,
        pricing_sync: None,
        prompt_cache: None,
    }
}

fn core_request(stream: bool, prompt: &str) -> CoreRequest {
    CoreRequest {
        model: ModelRef {
            requested: "claude-sonnet-4-6".to_owned(),
            upstream: None,
        },
        messages: vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: prompt.to_owned(),
                cache: None,
            }],
        }],
        system: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        sampling: SamplingOptions::default(),
        stream,
        metadata: Default::default(),
        provider_hints: Default::default(),
    }
}

fn body_hash_of(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn engine_request(
    stream: bool,
    prompt: &str,
    caller: &str,
    idempotency_key: Option<&str>,
) -> EngineRequest {
    let mut headers = std::collections::HashMap::new();
    if let Some(key) = idempotency_key {
        headers.insert("idempotency-key".to_owned(), key.to_owned());
    }
    EngineRequest::new(
        "fake",
        ProviderRouteKind::Messages,
        ClientDialect::Anthropic,
        core_request(stream, prompt),
        InboundRequest {
            auth: None,
            headers,
            wire: ClientWireRequest::new(
                ClientWireFormat::AnthropicMessages,
                serde_json::json!({}),
            ),
        },
    )
    .with_caller(caller)
    .with_body_hash(body_hash_of(prompt))
}

fn build_engine(endpoint: String, bus: Arc<RecordingBus>, cache: Option<MemoryCache>) -> Engine {
    let registry =
        ProviderRegistry::from_providers(vec![provider(endpoint)]).expect("provider registry");
    let mut builder = EngineBuilder::new(test_config())
        .providers(Arc::new(registry))
        .event_bus(bus as Arc<dyn EventBus>);
    if let Some(cache) = cache {
        builder = builder.cache(Arc::new(cache));
    }
    builder.build().expect("engine builds")
}

fn response_text(response: &llm_proxy_engine::EngineResponse) -> String {
    response
        .core
        .content
        .iter()
        .find_map(|content| match content {
            CoreContent::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn completed_rows(bus: &RecordingBus) -> Vec<(bool, String)> {
    bus.snapshot()
        .into_iter()
        .filter_map(|event| match event {
            ProxyEvent::ResponseCompleted(row) => Some((row.cache_hit, row.request_id)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn with_every_opt_in_off_nothing_is_stored_and_every_request_runs() {
    // Done-when 1: no cache, no idempotency — the default engine. Two
    // identical requests both reach the upstream (nothing was retained that
    // could answer or reject the second), and no recorded ledger row carries
    // prompt or response bytes.
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, Arc::clone(&bus), None);

    let first = engine
        .execute(engine_request(
            false,
            "secret prompt alpha",
            "caller-a",
            None,
        ))
        .await
        .expect("first request");
    let second = engine
        .execute(engine_request(
            false,
            "secret prompt alpha",
            "caller-a",
            None,
        ))
        .await
        .expect("second identical request is answered, not rejected");

    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    assert_eq!(response_text(&first), "hello from upstream");
    assert_eq!(response_text(&second), "hello from upstream");

    for event in bus.snapshot() {
        let serialized = serde_json::to_string(&event).expect("events serialize");
        assert!(
            !serialized.contains("secret prompt alpha"),
            "a ledger row carries the prompt: {serialized}"
        );
        assert!(
            !serialized.contains("hello from upstream"),
            "a ledger row carries the response: {serialized}"
        );
    }
    assert!(
        completed_rows(&bus).iter().all(|(hit, _)| !hit),
        "no row may claim a hit with every opt-in off"
    );
}

#[tokio::test]
async fn identical_concurrent_requests_share_one_upstream_call() {
    let upstream = CountingUpstream::new(Duration::from_millis(150));
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, Arc::clone(&bus), None);

    let mut requests = Vec::new();
    for i in 0..4 {
        let engine = engine.clone();
        requests.push(async move {
            engine
                .execute(
                    engine_request(false, "shared prompt", "caller-a", None)
                        .with_request_id(format!("req-{i}")),
                )
                .await
        });
    }
    let results = futures::future::join_all(requests).await;
    assert_eq!(results.len(), 4);
    for result in results {
        assert_eq!(
            response_text(&result.expect("joiner shares the leader's response")),
            "hello from upstream"
        );
    }

    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        1,
        "four identical concurrent requests from one caller must share one upstream call"
    );
    let rows = completed_rows(&bus);
    let hits = rows.iter().filter(|(hit, _)| *hit).count();
    let misses = rows.len() - hits;
    assert_eq!(misses, 1, "exactly one request made the upstream call");
    assert_eq!(hits, 3, "the three joiners are recorded as served copies");
}

#[tokio::test]
async fn single_flight_is_scoped_to_the_caller() {
    let upstream = CountingUpstream::new(Duration::from_millis(150));
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, bus, None);

    let a = engine.execute(engine_request(false, "same body", "caller-a", None));
    let b = engine.execute(engine_request(false, "same body", "caller-b", None));
    let (a, b) = futures::future::join(a, b).await;
    a.expect("caller a runs");
    b.expect("caller b runs");
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "one caller's in-flight request must not be shared with another caller"
    );
}

#[tokio::test]
async fn a_single_flight_leader_failure_fails_its_joiners() {
    let upstream = CountingUpstream::failing();
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, Arc::clone(&bus), None);

    let a = engine.execute(engine_request(false, "shared prompt", "caller-a", None));
    let b = engine.execute(engine_request(false, "shared prompt", "caller-a", None));
    let (a, b) = futures::future::join(a, b).await;
    assert!(a.is_err(), "the leader fails");
    assert!(matches!(b, Err(EngineError::Upstream { status: 500, .. })));
    assert!(
        completed_rows(&bus).is_empty(),
        "no completion rows for failed requests"
    );
}

#[tokio::test]
async fn identical_concurrent_streams_share_one_upstream_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let app = Router::new().route(
        "/{*path}",
        axum::routing::post(move || {
            let calls = Arc::clone(&counted);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(120)).await;
                let chunks = vec![
                    bytes::Bytes::from_static(
                        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    ),
                    bytes::Bytes::from_static(
                        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"shared\"}}\n\n",
                    ),
                    bytes::Bytes::from_static(
                        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    ),
                ];
                let body_stream =
                    futures::stream::iter(chunks).map(Ok::<_, std::convert::Infallible>);
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    )],
                    Body::from_stream(body_stream),
                )
                    .into_response()
            }
        }),
    );
    let endpoint = spawn_upstream(app).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, Arc::clone(&bus), None);

    let leader = engine.execute_stream(engine_request(true, "shared stream", "caller-a", None));
    let joiner = engine.execute_stream(
        engine_request(true, "shared stream", "caller-a", None).with_request_id("req-joiner"),
    );
    let (leader, joiner) = futures::future::join(leader, joiner).await;
    let leader = leader.expect("leader streams");
    let joiner = joiner.expect("joiner joins the shared stream");

    let collect = |stream: llm_proxy_engine::EngineStream| async move {
        let mut all = stream.head;
        let mut tail = stream.tail;
        while let Some(event) = tail.next().await {
            all.push(event);
        }
        all
    };
    let leader_events = collect(leader).await;
    let joiner_events = collect(joiner).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        leader_events, joiner_events,
        "the joiner sees exactly the leader's events, in order"
    );
    assert!(
        joiner_events
            .iter()
            .any(|event| matches!(event, CoreEvent::MessageStop { .. })),
        "the joiner's replay reaches the terminal: {joiner_events:?}"
    );

    // Both requests reconcile: one charged row (the leader) and one served
    // copy (the joiner).
    let rows = completed_rows(&bus);
    assert_eq!(rows.len(), 2, "leader and joiner each record a row");
    assert_eq!(rows.iter().filter(|(hit, _)| *hit).count(), 1);
}

#[tokio::test]
async fn a_cache_hit_answers_where_the_dedup_window_used_to_409() {
    // Done-when 2: identical request, answered from the cache with the same
    // body the first call produced — where the retired dedup window answered
    // 409.
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let cache = MemoryCache::new(Duration::from_secs(60), 16);
    let engine = build_engine(endpoint, Arc::clone(&bus), Some(cache));

    let first = engine
        .execute(engine_request(false, "cached prompt", "caller-a", None))
        .await
        .expect("first");
    let second = engine
        .execute(engine_request(false, "cached prompt", "caller-a", None))
        .await
        .expect("a cache hit succeeds where dedup used to answer 409");

    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    assert_eq!(response_text(&first), response_text(&second));
    assert_eq!(second.core.usage.input_tokens, 10);

    let rows = completed_rows(&bus);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|(hit, _)| *hit).count(), 1);
}

#[tokio::test]
async fn one_callers_cached_response_is_never_returned_to_another() {
    // Done-when 3: the cache key is caller-scoped.
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let cache = MemoryCache::new(Duration::from_secs(60), 16);
    let engine = build_engine(endpoint, Arc::clone(&bus), Some(cache));

    engine
        .execute(engine_request(false, "scoped prompt", "caller-a", None))
        .await
        .expect("caller a stores");
    engine
        .execute(engine_request(false, "scoped prompt", "caller-b", None))
        .await
        .expect("caller b must run its own request");
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "caller b may not be served caller a's stored response"
    );

    // And caller a's own second request is a hit.
    engine
        .execute(engine_request(false, "scoped prompt", "caller-a", None))
        .await
        .expect("caller a hits");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        completed_rows(&bus).iter().filter(|(hit, _)| *hit).count(),
        1
    );
}

#[tokio::test]
async fn expired_cache_entries_are_misses() {
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let cache = MemoryCache::new(Duration::from_millis(50), 16);
    let engine = build_engine(endpoint, Arc::clone(&bus), Some(cache));

    engine
        .execute(engine_request(false, "short ttl", "caller-a", None))
        .await
        .expect("first");
    tokio::time::sleep(Duration::from_millis(80)).await;
    engine
        .execute(engine_request(false, "short ttl", "caller-a", None))
        .await
        .expect("after expiry");
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "an entry past its TTL must not be served"
    );
}

#[tokio::test]
async fn the_cache_evicts_oldest_entries_at_its_bound() {
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let cache = MemoryCache::new(Duration::from_secs(60), 1);
    let engine = build_engine(endpoint, bus, Some(cache));

    engine
        .execute(engine_request(false, "first body", "caller-a", None))
        .await
        .expect("first stored");
    engine
        .execute(engine_request(false, "second body", "caller-a", None))
        .await
        .expect("second evicts the first at capacity 1");
    engine
        .execute(engine_request(false, "first body", "caller-a", None))
        .await
        .expect("the evicted body runs again");
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        3,
        "the first entry must have been evicted by the second"
    );
}

fn idempotency_config() -> AppConfig {
    let mut config = test_config();
    config.server.idempotency = IdempotencyConfig {
        enabled: true,
        ttl: Duration::from_secs(3600),
    };
    config
}

fn build_idempotent_engine(endpoint: String, bus: Arc<RecordingBus>) -> Engine {
    let registry =
        ProviderRegistry::from_providers(vec![provider(endpoint)]).expect("provider registry");
    EngineBuilder::new(idempotency_config())
        .providers(Arc::new(registry))
        .event_bus(bus as Arc<dyn EventBus>)
        .build()
        .expect("engine builds")
}

#[tokio::test]
async fn idempotency_key_replays_after_completion_and_409s_while_in_flight() {
    let upstream = CountingUpstream::new(Duration::from_millis(150));
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_idempotent_engine(endpoint, Arc::clone(&bus));

    // While the first is in flight, a matching retry is 409 — only then.
    let leader_engine = engine.clone();
    let first = tokio::spawn(async move {
        leader_engine
            .execute(engine_request(
                false,
                "idem body",
                "caller-a",
                Some("key-1"),
            ))
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let conflict = engine
        .execute(engine_request(
            false,
            "idem body",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect_err("an in-flight key conflicts");
    assert!(
        matches!(conflict, EngineError::IdempotencyInFlight),
        "got {conflict:?}"
    );
    assert_eq!(conflict.status_code(), 409);
    let first = first
        .await
        .expect("join of the spawned first request")
        .expect("first completes");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    // A retry after completion replays the stored response — no second call.
    let replay = engine
        .execute(engine_request(
            false,
            "idem body",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect("matching retry replays");
    assert_eq!(response_text(&replay), response_text(&first));
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    let rows = completed_rows(&bus);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|(hit, _)| *hit).count(), 1);
}

#[tokio::test]
async fn idempotency_keys_are_scoped_to_the_caller() {
    let upstream = CountingUpstream::new(Duration::from_millis(150));
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_idempotent_engine(endpoint, bus);

    let a = engine.execute(engine_request(
        false,
        "caller scoped",
        "caller-a",
        Some("key-1"),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let b = engine.execute(engine_request(
        false,
        "caller scoped",
        "caller-b",
        Some("key-1"),
    ));
    let (a, b) = futures::future::join(a, b).await;
    a.expect("caller a runs under its own key claim");
    b.expect("another caller's key claim must not 409 or replay for this one");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_key_reused_with_a_different_body_is_a_client_error() {
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_idempotent_engine(endpoint, bus);

    engine
        .execute(engine_request(
            false,
            "original body",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect("first");
    let error = engine
        .execute(engine_request(
            false,
            "different body",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect_err("a key cannot be reused with a different body");
    assert!(matches!(error, EngineError::InvalidRequest(_)));
    assert_eq!(error.status_code(), 400);
}

#[tokio::test]
async fn a_failed_first_request_releases_its_key() {
    let upstream = CountingUpstream::failing();
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_idempotent_engine(endpoint, bus);

    let first = engine
        .execute(engine_request(false, "retry me", "caller-a", Some("key-1")))
        .await;
    first.expect_err("the first attempt fails");

    // Failures are not stored: the retry executes again.
    let retry = engine
        .execute(engine_request(false, "retry me", "caller-a", Some("key-1")))
        .await;
    retry.expect_err("the retry fails too — but by executing, not replaying");
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "a retry after a failure must execute, not replay or 409"
    );
}

#[tokio::test]
async fn without_the_opt_in_the_idempotency_header_is_ignored() {
    let upstream = CountingUpstream::new(Duration::ZERO);
    let endpoint = spawn_upstream(upstream.router()).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_engine(endpoint, Arc::clone(&bus), None);

    engine
        .execute(engine_request(
            false,
            "ignored header",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect("first");
    engine
        .execute(engine_request(
            false,
            "ignored header",
            "caller-a",
            Some("key-1"),
        ))
        .await
        .expect("second runs again — the feature is off");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    assert!(
        completed_rows(&bus).iter().all(|(hit, _)| !hit),
        "no hit rows with the opt-in off"
    );
}

#[tokio::test]
async fn a_streaming_outcome_replays_as_a_stream_for_a_matching_retry() {
    let app = Router::new().route(
        "/{*path}",
        axum::routing::post(|| async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let chunks = vec![
                bytes::Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_i2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                ),
                bytes::Bytes::from_static(
                    b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"replayed\"}}\n\n",
                ),
                bytes::Bytes::from_static(
                    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            ];
            let body_stream = futures::stream::iter(chunks).map(Ok::<_, std::convert::Infallible>);
            (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                )],
                Body::from_stream(body_stream),
            )
                .into_response()
        }),
    );
    let endpoint = spawn_upstream(app).await;
    let bus = Arc::new(RecordingBus::new());
    let engine = build_idempotent_engine(endpoint, Arc::clone(&bus));

    let first = engine
        .execute_stream(engine_request(
            true,
            "stream idem",
            "caller-a",
            Some("key-s"),
        ))
        .await
        .expect("first streams");
    let mut events = first.head;
    let mut tail = first.tail;
    while let Some(event) = tail.next().await {
        events.push(event);
    }
    tokio::time::sleep(Duration::from_millis(30)).await;

    let replay = engine
        .execute_stream(engine_request(
            true,
            "stream idem",
            "caller-a",
            Some("key-s"),
        ))
        .await
        .expect("matching streaming retry replays the stored events");
    let mut replayed = replay.head;
    let mut tail = replay.tail;
    while let Some(event) = tail.next().await {
        replayed.push(event);
    }
    assert_eq!(
        events, replayed,
        "the replay yields exactly the stored events"
    );

    let rows = completed_rows(&bus);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|(hit, _)| *hit).count(), 1);
}
