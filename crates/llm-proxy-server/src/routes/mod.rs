use std::sync::Arc;
use std::time::Instant;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tower::ServiceBuilder;
use tower_http::{
    catch_panic::CatchPanicLayer, cors::CorsLayer, set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer, trace::TraceLayer,
};
use tracing::info_span;

use crate::middleware::{CallerId, RequestId, RequestIdGenerator, extract_bearer_token};
use crate::state::AppState;
use llm_proxy_storage::EventBus;
use secrecy::ExposeSecret;

mod chat;
mod core_pipeline;
mod error_response;
mod health;
mod messages;
mod models;
mod token_count;

use chat::handle_chat_completions;
use health::{health, ready, version};
use messages::handle_messages;
use models::handle_models;
use token_count::count_tokens;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const NOT_FOUND_BODY_DRAIN_LIMIT: usize = 1024;

/// Build the full HTTP router: `/health`, `/ready`, `/version`, provider API
/// routes under `/providers/{provider}/v1/*`, and the 404/405 fallbacks. The
/// admin `/usage` surface merges in only under the `admin_on_main_listener` opt-in.
pub fn router(state: AppState) -> Router {
    let timeout = state.request_timeout();
    let id_gen = Arc::clone(&state.request_id_gen);
    let event_bus = Arc::clone(&state.event_bus);
    let state_arc = state.clone();
    let trace = TraceLayer::new_for_http().make_span_with(make_request_span);
    let lightweight = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/version", get(version))
        .layer(
            ServiceBuilder::new()
                .layer(from_fn_with_state(Arc::clone(&id_gen), inject_request_id))
                .layer(trace.clone()),
        );
    let api_middleware = ServiceBuilder::new()
        .layer(from_fn_with_state(id_gen, inject_request_id))
        .layer(from_fn_with_state(state_arc, require_client_auth))
        .layer(from_fn_with_state(event_bus, normalize_error_responses))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(trace)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ));

    let api = Router::new()
        .route("/providers/{provider}/v1/messages", post(handle_messages))
        .route(
            "/providers/{provider}/v1/messages/count_tokens",
            post(count_tokens),
        )
        .route(
            "/providers/{provider}/v1/chat/completions",
            post(handle_chat_completions),
        )
        .route("/providers/{provider}/v1/models", get(handle_models))
        .layer(api_middleware);

    let router = Router::new()
        .merge(lightweight)
        .merge(api)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    let usage_on_main = match (
        crate::admin_router(&state),
        state.app_config.server.admin_on_main_listener,
    ) {
        (Some(usage), true) => Some(usage),
        _ => None,
    };
    let router = match cors_layer_for(state.allowed_origins()) {
        Some(cors) => router.layer(cors),
        None => router,
    };

    let router: Router = router
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(CatchPanicLayer::new())
        .with_state(state);
    match usage_on_main {
        Some(usage) => router.merge(usage),
        None => router,
    }
}
fn cors_layer_for(origins: Option<&[String]>) -> Option<CorsLayer> {
    let origins = origins?;
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| match origin.parse::<HeaderValue>() {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, origin, "skipping invalid server.allowed_origins entry");
                None
            }
        })
        .collect();
    if parsed.is_empty() {
        return None;
    }
    let methods = [Method::GET, Method::POST, Method::OPTIONS];
    let headers = [
        header::CONTENT_TYPE,
        header::AUTHORIZATION,
        header::ACCEPT,
        header::USER_AGENT,
        HeaderName::from_static("x-api-key"),
        HeaderName::from_static("anthropic-version"),
        HeaderName::from_static("anthropic-beta"),
        HeaderName::from_static("x-goog-api-key"),
        HeaderName::from_static("x-request-id"),
    ];
    Some(
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods(methods)
            .allow_headers(headers),
    )
}
async fn inject_request_id(
    State(id_gen): State<Arc<RequestIdGenerator>>,
    mut req: Request,
    next: Next,
) -> Response {
    let id = id_gen.next_id();
    req.extensions_mut().insert(RequestId(id));
    next.run(req).await
}
async fn require_client_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !state.client_auth.is_enforced() {
        let mut req = req;
        req.extensions_mut().insert(CallerId::anonymous());
        return next.run(req).await;
    }
    let connect_info = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0);
    let client_ip = crate::middleware::get_client_ip(
        req.headers(),
        connect_info.as_ref(),
        state.trust_forwarded_headers(),
    );

    let authenticated = match extract_bearer_token(req.headers()) {
        Some(token) => {
            if !state.rate_limiter.would_allow(&client_ip) {
                state.metrics.record_rate_limited();
                None
            } else {
                state.client_auth.authenticate(token.expose_secret()).await
            }
        }
        None => None,
    };

    let mut req = req;
    match authenticated {
        Some(key_id) => {
            tracing::debug!(caller = %key_id, "client authenticated");
            req.extensions_mut().insert(CallerId::authenticated(key_id));
            next.run(req).await
        }
        None => {
            if !state.rate_limiter.is_allowed(&client_ip) {
                state.metrics.record_rate_limited();
            }
            let request_id = req
                .extensions()
                .get::<RequestId>()
                .cloned()
                .unwrap_or_else(|| RequestId("unknown".to_owned()));
            let path = req.uri().path().to_owned();
            if let Err(e) = axum::body::to_bytes(req.into_body(), NOT_FOUND_BODY_DRAIN_LIMIT).await
            {
                tracing::trace!(error = %e, "draining body of auth-rejected request failed");
            }
            let error = error_response::RouteError::Unauthorized;
            core_pipeline::emit_response_failed(
                &state.event_bus,
                &request_id.0,
                None,
                None,
                &error,
                std::time::Instant::now(),
            );
            let mut response =
                error_response::route_error_response(protocol_for_path(&path), &error);
            if let Ok(value) = request_id.0.parse() {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
    }
}
fn make_request_span(req: &Request) -> tracing::Span {
    let id = req.extensions().get::<RequestId>();
    info_span!(
        "request",
        request_id = %id.cloned().unwrap_or_else(|| RequestId(String::new())),
        method = %req.method(),
        path = %req.uri().path(),
    )
}
async fn normalize_error_responses(
    State(event_bus): State<Arc<dyn EventBus>>,
    req: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let protocol = protocol_for_path(req.uri().path());
    let request_id = req.extensions().get::<RequestId>().cloned();
    let response = next.run(req).await;
    let status = response.status();

    if status != StatusCode::REQUEST_TIMEOUT && status != StatusCode::PAYLOAD_TOO_LARGE {
        return response;
    }
    if let Err(e) = axum::body::to_bytes(response.into_body(), NOT_FOUND_BODY_DRAIN_LIMIT).await {
        tracing::trace!(error = %e, "draining layer rejection body failed");
    }

    let error = if status == StatusCode::REQUEST_TIMEOUT {
        error_response::RouteError::RequestTimeout
    } else {
        error_response::RouteError::PayloadTooLarge
    };
    let id_str = request_id
        .as_ref()
        .map(|id| id.0.as_str())
        .unwrap_or("layer-rejection");
    core_pipeline::emit_response_failed(&event_bus, id_str, None, None, &error, start);

    let mut rewritten = error_response::route_error_response(protocol, &error);
    if let Some(id) = request_id {
        if let Ok(value) = id.0.parse() {
            rewritten.headers_mut().insert("x-request-id", value);
        }
    }
    rewritten
}
fn protocol_for_path(path: &str) -> error_response::ClientProtocol {
    let is_openai_chat_path =
        path.contains("/v1/chat/completions") || path.contains("/v1/chat/edits");
    if is_openai_chat_path {
        error_response::ClientProtocol::OpenAiChat
    } else {
        error_response::ClientProtocol::Anthropic
    }
}

async fn not_found(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let start = Instant::now();
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(|| RequestId("unknown".to_owned()));

    let path = req.uri().path().to_owned();
    if let Err(e) = axum::body::to_bytes(req.into_body(), NOT_FOUND_BODY_DRAIN_LIMIT).await {
        tracing::trace!(error = %e, "body drain in 404 handler failed");
    }

    let error = error_response::RouteError::NotFound;
    core_pipeline::emit_response_failed(&state.event_bus, &request_id.0, None, None, &error, start);

    let protocol = protocol_for_path(&path);
    let mut response = error_response::route_error_response(protocol, &error);
    if let Ok(value) = request_id.0.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}
async fn method_not_allowed(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let start = Instant::now();
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(|| RequestId("unknown".to_owned()));

    let path = req.uri().path().to_owned();
    if let Err(e) = axum::body::to_bytes(req.into_body(), NOT_FOUND_BODY_DRAIN_LIMIT).await {
        tracing::trace!(error = %e, "body drain in 405 handler failed");
    }

    let error = error_response::RouteError::MethodNotAllowed;
    core_pipeline::emit_response_failed(&state.event_bus, &request_id.0, None, None, &error, start);

    let protocol = protocol_for_path(&path);
    let mut response = error_response::route_error_response(protocol, &error);
    if let Ok(value) = request_id.0.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::cors_layer_for;

    #[test]
    fn cors_layer_absent_when_origins_unset() {
        assert!(cors_layer_for(None).is_none());
    }

    #[test]
    fn cors_layer_absent_when_origins_empty() {
        let empty: Vec<String> = Vec::new();
        assert!(cors_layer_for(Some(&empty)).is_none());
    }

    #[test]
    fn cors_layer_absent_when_every_origin_is_invalid() {
        let bad = vec!["bad\norigin".to_owned(), "\0".to_owned()];
        assert!(cors_layer_for(Some(&bad)).is_none());
    }

    #[test]
    fn cors_layer_present_when_origin_configured() {
        let origins = vec!["https://app.example.com".to_owned()];
        assert!(cors_layer_for(Some(&origins)).is_some());
    }

    #[test]
    fn cors_layer_keeps_valid_origins_while_skipping_invalid() {
        let origins = vec![
            "bad\norigin".to_owned(),
            "https://app.example.com".to_owned(),
        ];
        assert!(cors_layer_for(Some(&origins)).is_some());
    }
}
#[cfg(test)]
mod auth_gate_tests {
    use super::*;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    fn auth_router(rate_limit_rpm: u32) -> (Router, AppState) {
        use argon2::{Algorithm, Argon2, CustomizedPasswordHasher, Params, Version};

        let params = Params::new(8, 1, 1, None).expect("valid test params");
        let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone());
        let phc = hasher
            .hash_password_customized(
                b"sekrit-token",
                b"0123456789abcdef",
                Some(&argon2::ARGON2ID_IDENT),
                None,
                params,
            )
            .expect("hash token")
            .to_string();

        let mut clients = std::collections::HashMap::new();
        clients.insert("laptop".to_owned(), secrecy::SecretString::from(phc));
        let app_config = llm_proxy_core::AppConfig {
            server: llm_proxy_core::ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: std::time::Duration::from_secs(300),
                shutdown_timeout: std::time::Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "gate-test".to_owned(),
                rate_limit_rpm,
                trust_forwarded_headers: false,
                dedup_window: std::time::Duration::ZERO,
                log_format: Default::default(),
                admin_bind: None,
                admin_on_main_listener: false,
            },
            auth: llm_proxy_core::AuthConfig {
                mode: llm_proxy_core::AuthMode::ClientKeys,
                clients,
                admin: None,
            },
            storage: None,
        };

        let state = crate::state::AppState::new(
            app_config,
            llm_proxy_core::ProviderRegistry::from_providers(vec![]).expect("empty registry"),
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::new(),
            crate::state::BuildInfo {
                name: "test",
                version: "0.0.0",
                target: "test",
                git_sha: "test",
            },
        );

        let router = Router::new()
            .route(
                "/providers/x/v1/messages",
                post(|| async { StatusCode::OK }),
            )
            .layer(from_fn_with_state(
                state.clone(),
                super::require_client_auth,
            ));
        (router, state)
    }

    fn bearer_request(token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/providers/x/v1/messages")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn valid_token_passes_when_the_peer_has_budget() {
        let (app, _state) = auth_router(100);
        let response = app.oneshot(bearer_request("sekrit-token")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn exhausted_failure_budget_gates_verification_before_argon2() {
        let (app, state) = auth_router(1);
        let first = app
            .clone()
            .oneshot(bearer_request("wrong-token"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state.metrics.get_snapshot().rate_limited, 0);
        let second = app.oneshot(bearer_request("sekrit-token")).await.unwrap();
        assert_eq!(
            second.status(),
            StatusCode::UNAUTHORIZED,
            "a peer with an exhausted failure budget must be gated before verification"
        );
        assert!(
            state.metrics.get_snapshot().rate_limited >= 1,
            "the gate records a limiter rejection"
        );
    }
}
