//! Host/Origin guard integration tests (plan 07 step 1, backlog 11): under
//! anonymous auth a request with a foreign `Host`, `Origin`, or hostile
//! `Sec-Fetch-Site` is refused, while loopback callers pass.

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use llm_proxy_adapters::ProviderAdapterRegistry;
use llm_proxy_core::{
    AppConfig, AuthConfig, AuthMode, AuthStyle, ProviderAdapterConfig, ProviderConfig,
    ProviderRegistry, ProviderRoutesConfig, ServerConfig,
};
use llm_proxy_server::{AppState, BuildInfo, build_router};
use llm_proxy_transport::ProxyClient;
use serde_json::{Value, json};
use tower::ServiceExt;

#[cfg(test)]
mod tests {
    use super::*;

    fn app_config(allowed_origins: Option<Vec<String>>, auth: AuthConfig) -> AppConfig {
        AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: Duration::from_secs(300),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins,
                server_name: "host-guard-test".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                cache: Default::default(),
                idempotency: Default::default(),
                log_format: Default::default(),
                admin_bind: None,
                admin_on_main_listener: false,
                endpoint_families: Default::default(),
                upstream_timeouts: Default::default(),
                bare_v1: Default::default(),
                preflight: Default::default(),
            },
            auth,
            storage: None,
        }
    }

    fn state(allowed_origins: Option<Vec<String>>, auth: AuthConfig) -> AppState {
        let provider = ProviderConfig {
            name: "mock".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::XApiKey,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "messages".to_owned(),
                ProviderAdapterConfig {
                    protocol: "anthropic_messages".to_owned(),
                    // An unreachable loopback upstream: the handler's own error
                    // (a 5xx) proves the request passed the guard and reached
                    // the engine.
                    endpoint: "https://127.0.0.1:9/v1/messages".to_owned(),
                    headers: std::sync::Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
                },
            )]),
            routes: ProviderRoutesConfig {
                messages: Some("messages".to_owned()),
                chat_completions: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        AppState::new(
            app_config(allowed_origins, auth),
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

    fn post_messages(extra: &[(&'static str, &'static str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/providers/mock/v1/messages")
            .header("content-type", "application/json");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        builder
            .body(Body::from(
                json!({
                    "model": "claude-sonnet-4-6",
                    "messages": [{ "role": "user", "content": "hi" }],
                    "max_tokens": 8
                })
                .to_string(),
            ))
            .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn a_request_without_origin_passes_under_anonymous_auth() {
        let app = build_router(state(None, AuthConfig::default()));
        let response = app.oneshot(post_messages(&[])).await.unwrap();
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a non-browser loopback caller must not be refused by the host guard"
        );
    }

    #[tokio::test]
    async fn a_rebound_host_is_refused_under_anonymous_auth() {
        let app = build_router(state(None, AuthConfig::default()));
        let response = app
            .clone()
            .oneshot(post_messages(&[("host", "rebind.attacker.example:3456")]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_eq!(
            body["error"]["type"], "forbidden_request_source",
            "the refusal must identify itself: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Host")),
            "the refusal must name the refused header: {body}"
        );
    }

    #[tokio::test]
    async fn a_browser_request_with_a_foreign_origin_is_refused() {
        let app = build_router(state(None, AuthConfig::default()));
        // A browser scripted request carries Fetch Metadata alongside Origin.
        let response = app
            .clone()
            .oneshot(post_messages(&[
                ("origin", "https://app.attacker.example"),
                ("sec-fetch-site", "same-site"),
                ("sec-fetch-mode", "cors"),
            ]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Origin")),
            "the refusal must name the refused header: {body}"
        );
    }

    /// Captured CLI traffic (the contract-replay fixtures) carries an `Origin`
    /// header with no browser markers; the guard must not refuse it.
    #[tokio::test]
    async fn a_non_browser_request_with_an_origin_header_passes() {
        let app = build_router(state(None, AuthConfig::default()));
        let response = app
            .oneshot(post_messages(&[("origin", "https://evil.example")]))
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a non-browser client's stray Origin header must not be refused (the \
             Host check already pinned it to this listener)"
        );
    }

    #[tokio::test]
    async fn a_cross_site_fetch_metadata_request_is_refused() {
        let app = build_router(state(None, AuthConfig::default()));
        for site in ["cross-site", "same-site"] {
            let response = app
                .clone()
                .oneshot(post_messages(&[("sec-fetch-site", site)]))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "Sec-Fetch-Site: {site} is another site's script"
            );
        }
        // The proxy's own page and user navigations pass.
        for site in ["same-origin", "none"] {
            let response = app
                .clone()
                .oneshot(post_messages(&[("sec-fetch-site", site)]))
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::FORBIDDEN,
                "Sec-Fetch-Site: {site} must be admitted"
            );
        }
    }

    #[tokio::test]
    async fn the_operators_listed_origin_is_admitted_for_browser_requests() {
        let app = build_router(state(
            Some(vec!["https://app.mycompany.example".to_owned()]),
            AuthConfig::default(),
        ));
        let response = app
            .clone()
            .oneshot(post_messages(&[
                ("origin", "https://app.mycompany.example"),
                ("sec-fetch-site", "cross-site"),
            ]))
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an origin the operator listed in server.allowed_origins must pass"
        );

        // Any other origin still refuses.
        let response = app
            .oneshot(post_messages(&[
                ("origin", "https://other.example"),
                ("sec-fetch-site", "cross-site"),
            ]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Client-keys mode carries a credential a browser page does not hold, so the
    /// guard stands down there: the request reaches the auth middleware instead.
    #[tokio::test]
    async fn the_guard_stands_down_under_client_keys_auth() {
        let auth = AuthConfig {
            mode: AuthMode::ClientKeys,
            clients: std::collections::HashMap::from([(
                "laptop".to_owned(),
                llm_proxy_core::ClientKeySpec::Verifier(secrecy::SecretString::from(
                    "$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA",
                )),
            )]),
            admin: None,
        };
        let app = build_router(state(None, auth));
        let response = app
            .oneshot(post_messages(&[
                ("host", "rebind.attacker.example:3456"),
                ("origin", "https://app.attacker.example"),
            ]))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "under client keys the auth middleware answers, not the host guard"
        );
    }

    /// The guard must not break the plain HTTP plumbing axum depends on: a
    /// default Host header (loopback, set by every HTTP client) passes.
    #[tokio::test]
    async fn a_loopback_host_header_passes() {
        let app = build_router(state(None, AuthConfig::default()));
        for host in ["127.0.0.1:3456", "localhost:3456", "[::1]:3456"] {
            let response = app
                .clone()
                .oneshot(post_messages(&[("host", host)]))
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::FORBIDDEN,
                "Host {host} names this listener and must pass"
            );
        }
    }

    /// Sanity for the "reaches the engine" claims above: without any guard
    /// headers the same request's status comes from the engine (an upstream
    /// error), proving 403 responses were produced by the guard and nothing else.
    #[tokio::test]
    async fn unguarded_requests_reach_the_engine_and_fail_upstream() {
        let app = build_router(state(None, AuthConfig::default()));
        let response = app.oneshot(post_messages(&[])).await.unwrap();
        assert!(
            response.status().is_server_error(),
            "the unreachable upstream must produce a 5xx from the engine, got {}",
            response.status()
        );
    }
}
