//! Non-chat endpoint families (plan 09 step 7, audit conflict 16) end to
//! end: each family handler resolves its route through the provider
//! registry, admits through the same step the chat pipeline uses,
//! forwards with the provider's credential and the surgical edit (model
//! aliasing), and writes usage rows. Audio and file bodies pass through
//! verbatim; unknown JSON fields survive.

#![cfg(test)]
#![cfg(feature = "endpoint-families")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use llm_proxy_adapters::ProviderAdapterRegistry;
use llm_proxy_core::{
    AppConfig, AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRegistry,
    ProviderRouteKind, ProviderRoutesConfig, ServerConfig,
};
use llm_proxy_server::{AppState, BuildInfo, build_router};
use llm_proxy_storage::{ProxyEvent, RecordingBus};
use llm_proxy_transport::ProxyClient;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Records one (path, auth, content_type, body) per upstream request.
type Capture = Arc<Mutex<Vec<(String, String, String, Bytes)>>>;

async fn spawn_upstream() -> (String, Capture) {
    let captured: Capture = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let app = Router::new().route(
        "/{*path}",
        axum::routing::post(
            move |path: axum::extract::Path<String>,
                  headers: axum::http::HeaderMap,
                  body: Bytes| {
                let sink = Arc::clone(&sink);
                async move {
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let content_type = headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    sink.lock().await.push((
                        path.to_string(),
                        auth,
                        content_type,
                        body.clone(),
                    ));
                    // Echo a family-shaped JSON answer.
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"model":"text-embedding-3-small","usage":{"prompt_tokens":5,"total_tokens":5}}"#,
                    )
                        .into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::trace!(%error, "fake upstream stopped");
        }
    });
    (format!("http://{addr}/v1"), captured)
}

fn provider_config(endpoint: &str) -> ProviderConfig {
    let mut adapters: HashMap<String, ProviderAdapterConfig> = HashMap::new();
    for family in [
        ProviderRouteKind::Embeddings,
        ProviderRouteKind::Rerank,
        ProviderRouteKind::AudioTranscriptions,
        ProviderRouteKind::AudioSpeech,
        ProviderRouteKind::ImagesGenerations,
        ProviderRouteKind::Files,
        ProviderRouteKind::Batches,
    ] {
        adapters.insert(
            format!("{family:?}"),
            ProviderAdapterConfig {
                protocol: "openai_chat_completions".to_owned(),
                endpoint: endpoint.to_owned(),
                headers: Arc::new(HashMap::new()),
                forward_inbound_headers: None,
                lossy: false,
            },
        );
    }
    let mut model_aliases = HashMap::new();
    model_aliases.insert(
        "embed-small".to_owned(),
        "text-embedding-3-small".to_owned(),
    );
    ProviderConfig {
        name: "fake".to_owned(),
        api_key: secrecy::SecretString::from("sk-upstream-key"),
        auth_style: AuthStyle::Bearer,
        passthrough_auth: false,
        api_keys: None,
        retry: None,
        egress: None,
        adapters,
        routes: ProviderRoutesConfig {
            messages: None,
            chat_completions: None,
            responses: None,
            generate_content: None,
            embeddings: Some("Embeddings".to_owned()),
            rerank: Some("Rerank".to_owned()),
            audio_transcriptions: Some("AudioTranscriptions".to_owned()),
            audio_speech: Some("AudioSpeech".to_owned()),
            images_generations: Some("ImagesGenerations".to_owned()),
            files: Some("Files".to_owned()),
            batches: Some("Batches".to_owned()),
        },
        model_aliases,
        discovery: None,
        catalog: None,
        pricing: Default::default(),
        concurrency: None,
        pricing_sync: None,
        prompt_cache: None,
    }
}

fn state(config: AppConfig, registry: ProviderRegistry, bus: Arc<RecordingBus>) -> AppState {
    AppState::new(
        config,
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
    .with_event_bus(bus)
}

fn base_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("bind"),
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            log_level: "info".to_owned(),
            hot_reload: false,
            allowed_origins: None,
            server_name: "families-test".to_owned(),
            rate_limit_rpm: 10_000,
            trust_forwarded_headers: false,
            log_format: Default::default(),
            admin_bind: None,
            admin_on_main_listener: false,
            endpoint_families: Default::default(),
            upstream_timeouts: Default::default(),
            bare_v1: Default::default(),
            preflight: Default::default(),
            semantic_cache: Default::default(),
            cache: Default::default(),
            idempotency: Default::default(),
            metrics: Default::default(),
        },
        auth: Default::default(),
        storage: None,
        replica: None,
        mcp: None,
    }
}

async fn fixture() -> (Router, Capture, Arc<RecordingBus>) {
    let (endpoint, captured) = spawn_upstream().await;
    let registry = ProviderRegistry::from_providers(vec![provider_config(&endpoint)])
        .expect("registry builds");
    let bus = Arc::new(RecordingBus::new());
    let app = build_router(state(base_config(), registry, Arc::clone(&bus)));
    (app, captured, bus)
}

#[tokio::test]
async fn embeddings_routes_through_with_the_model_alias_and_credential() {
    let (app, captured, bus) = fixture().await;
    let body = r#"{"model":"embed-small","input":["hello"],"vendor_extra_field":true}"#;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/providers/fake/v1/embeddings")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);

    let requests = captured.lock().await;
    let (path, auth, content_type, upstream_body) = &requests[0];
    assert_eq!(path, "v1", "routed to the configured family endpoint");
    assert_eq!(auth, "Bearer sk-upstream-key", "the provider credential");
    assert!(content_type.starts_with("application/json"));
    let parsed: serde_json::Value = serde_json::from_slice(upstream_body).expect("json");
    assert_eq!(
        parsed["model"].as_str(),
        Some("text-embedding-3-small"),
        "the requested model is aliased for egress"
    );
    assert_eq!(
        parsed["vendor_extra_field"].as_bool(),
        Some(true),
        "unknown fields survive (the same-dialect rule)"
    );
    drop(requests);

    // Usage rows: family-scoped route kind, the requested model kept.
    let events = bus.snapshot();
    let request_row = events
        .iter()
        .find_map(|event| match event {
            ProxyEvent::RequestReceived(row) => Some(row),
            _ => None,
        })
        .expect("request row");
    assert_eq!(request_row.route_kind, "embeddings");
    assert_eq!(request_row.model.requested, "embed-small");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProxyEvent::ResponseCompleted(_)))
    );
}

#[tokio::test]
async fn rerank_and_images_and_batches_share_the_spine() {
    let (app, captured, _bus) = fixture().await;
    for uri in [
        "/providers/fake/v1/rerank",
        "/providers/fake/v1/images/generations",
        "/providers/fake/v1/batches",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"model":"embed-small"}"#))
                    .expect("request"),
            )
            .await
            .expect("served");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    assert_eq!(captured.lock().await.len(), 3, "each family routed once");
}

#[tokio::test]
async fn audio_bodies_pass_through_verbatim_with_their_content_type() {
    let (app, captured, _bus) = fixture().await;
    // A synthetic multipart body; the proxy must not rewrite it.
    let multipart = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\nRIFF-bytes\r\n--boundary--\r\n";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/providers/fake/v1/audio/transcriptions")
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=boundary",
                )
                .body(Body::from(multipart))
                .expect("request"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    let requests = captured.lock().await;
    let (_, _, content_type, body) = &requests[0];
    assert!(
        content_type.starts_with("multipart/form-data"),
        "the client's boundary survives, got {content_type}"
    );
    assert!(
        body.windows(b"RIFF-bytes".len())
            .any(|window| window == b"RIFF-bytes"),
        "audio bytes are forwarded verbatim"
    );
}

#[tokio::test]
async fn file_subpaths_are_appended_to_the_family_endpoint() {
    let (app, captured, _bus) = fixture().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/providers/fake/v1/files/file-abc/content")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from("file-bytes"))
                .expect("request"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    let requests = captured.lock().await;
    let (path, _, _, body) = &requests[0];
    assert_eq!(
        path.as_str(),
        "v1/file-abc/content",
        "the captured tail is appended to the family endpoint"
    );
    assert_eq!(&body[..], b"file-bytes");
}

#[tokio::test]
async fn an_unconfigured_family_answers_404_with_a_named_route() {
    // A registry with no family routes: the family kind is unconfigured.
    let (endpoint, _captured) = spawn_upstream().await;
    let mut config = provider_config(&endpoint);
    config.routes.embeddings = None;
    let registry = ProviderRegistry::from_providers(vec![config]).expect("registry");
    let bus = Arc::new(RecordingBus::new());
    let app = build_router(state(base_config(), registry, Arc::clone(&bus)));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/providers/fake/v1/embeddings")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"model":"embed-small"}"#))
                .expect("request"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The refusal is accounted.
    assert!(
        matches!(bus.snapshot().first(), Some(ProxyEvent::ResponseFailed(_))),
        "unconfigured-family refusals write a failure row"
    );
}
