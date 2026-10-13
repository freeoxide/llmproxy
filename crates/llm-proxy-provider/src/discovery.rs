//! Upstream provider model discovery.

use std::time::Duration;

use futures::TryStreamExt;
use llm_proxy_core::{
    AuthStyle, ProviderConfig, ProviderDiscoveryConfig, ProviderDiscoveryKind, ProviderRouteKind,
    StaticModelCatalogEntry,
};
use reqwest::{Client, RequestBuilder, Url};
use secrecy::ExposeSecret;
use serde_json::Value;

use crate::ProviderError;

const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Maximum number of bytes streamed from a discovery error body before it is
/// passed to [`ProviderError::api`].
///
/// Streamed (not buffered-then-truncated) so an adversarial discovery endpoint
/// returning a large error body cannot spike transient memory (audit
/// discovery-error-body-full-read).
const MAX_DISCOVERY_ERROR_BODY_BYTES: usize = 2048;

/// GET-capable HTTP client for provider model discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryClient {
    http: Client,
}

impl DiscoveryClient {
    /// Build a discovery client with short, independent timeouts.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be constructed.
    pub fn try_new() -> Result<Self, ProviderError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many discovery redirects");
                }
                let next = attempt.url();
                if !matches!(next.scheme(), "http" | "https") {
                    return attempt.stop();
                }
                // SSRF / credential-disclosure guard: reqwest strips
                // `Authorization` on cross-host redirects, but it does NOT strip
                // custom auth headers such as `x-api-key` / `x-goog-api-key`
                // (the credentials used by Anthropic and Gemini discovery).
                // Refuse to follow a redirect whose host/port/scheme differs from
                // the original request's so a compromised or MITM discovery
                // endpoint cannot replay the provider API key to an attacker host.
                if let Some(origin) = attempt.previous().first() {
                    let cross_host = next.host_str() != origin.host_str()
                        || next.port_or_known_default() != origin.port_or_known_default()
                        || next.scheme() != origin.scheme();
                    if cross_host {
                        return attempt.error("discovery redirect leaves the origin host");
                    }
                }
                attempt.follow()
            }))
            .build()?;
        Ok(Self { http })
    }

    /// Fetch and normalize the configured provider model list.
    ///
    /// # Errors
    ///
    /// Returns an error for missing discovery config, invalid responses,
    /// pagination failures, or transport errors.
    pub async fn discover(
        &self,
        provider: &ProviderConfig,
    ) -> Result<Vec<StaticModelCatalogEntry>, ProviderError> {
        let discovery = provider.discovery.as_ref().ok_or_else(|| {
            ProviderError::InvalidConfig(format!(
                "provider \"{}\" has no discovery configuration",
                provider.name
            ))
        })?;

        let mut url = Url::parse(&discovery.endpoint).map_err(|error| {
            ProviderError::InvalidConfig(format!("invalid discovery endpoint: {error}"))
        })?;
        let mut models = Vec::new();
        let mut dropped = 0usize;
        // Track the previous pagination token so a non-advancing token (the
        // upstream returns the same `nextPageToken` / `after_id` every page) is
        // detected and pagination stops early with a warning, instead of
        // fetching the same page up to `max_pages` times (audit
        // discovery-nonadvancing-pagination-token).
        let mut previous_token: Option<String> = None;

        for _ in 0..discovery.max_pages {
            let value = self.fetch_page(provider, &url).await?;
            dropped += parse_models(discovery.kind, &value, &mut models)?;
            if models.len() > discovery.max_models {
                return Err(ProviderError::InvalidConfig(format!(
                    "provider \"{}\" discovery exceeded {} models",
                    provider.name, discovery.max_models
                )));
            }

            let Some((name, token)) = next_page(discovery.kind, &value) else {
                models.sort_by(|a, b| a.id.cmp(&b.id));
                models.dedup_by(|a, b| a.id == b.id);
                // Surface the aggregate count of records dropped across all
                // pages so a silently partial catalog is observable in the
                // success path (GAP-LOW-6). Logged at `info!` only when
                // non-zero to avoid noise on clean responses.
                if dropped > 0 {
                    tracing::info!(
                        provider = %provider.name,
                        kept = models.len(),
                        dropped,
                        "discovery completed with malformed or filtered model records",
                    );
                }
                return Ok(models);
            };

            // Stop early if the upstream returned a token identical to the one
            // we just used: feeding it back would re-fetch the same page in a
            // tight loop until `max_pages`. Sort/dedup the (possibly partial)
            // catalog and return it rather than treating this as a hard error,
            // since the records gathered so far are valid (audit
            // discovery-nonadvancing-pagination-token).
            if previous_token.as_deref() == Some(token.as_str()) {
                tracing::warn!(
                    provider = %provider.name,
                    token = %token,
                    "discovery pagination token did not advance; stopping early \
                     to avoid repeated identical page fetches",
                );
                models.sort_by(|a, b| a.id.cmp(&b.id));
                models.dedup_by(|a, b| a.id == b.id);
                return Ok(models);
            }
            previous_token = Some(token.clone());

            set_query_parameter(&mut url, name, &token);
        }

        Err(ProviderError::InvalidConfig(format!(
            "provider \"{}\" discovery exceeded {} pages",
            provider.name, discovery.max_pages
        )))
    }

    async fn fetch_page(
        &self,
        provider: &ProviderConfig,
        url: &Url,
    ) -> Result<Value, ProviderError> {
        let discovery = provider.discovery.as_ref().ok_or_else(|| {
            ProviderError::InvalidConfig("missing discovery configuration".to_owned())
        })?;

        // Bounded retry with fixed backoff for transient failures (GAP-LOW-4):
        // a single 5xx or transport hiccup (TLS reset, connection reset,
        // timeout) mid-pagination would otherwise abort the whole catalog
        // refresh for a provider. Only server-side / transport errors are
        // retried — 4xx and parse errors are surfaced immediately. The total
        // extra wall-clock is bounded by `MAX_TRANSIENT_RETRIES * BACKOFF`.
        let mut last_error = None;
        for attempt in 0..=Self::MAX_TRANSIENT_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Self::RETRY_BACKOFF).await;
            }
            let request = self.build_request(provider, discovery, url);
            match self.send_once(request, discovery.max_response_bytes).await {
                Ok(value) => return Ok(value),
                Err(error) if Self::is_transient(&error) => {
                    tracing::warn!(
                        provider = %provider.name,
                        attempt,
                        max_attempts = Self::MAX_TRANSIENT_RETRIES,
                        error = %error,
                        "transient discovery failure; retrying",
                    );
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        // All retries exhausted; surface the last transient error.
        Err(last_error.expect("retry loop ran at least once"))
    }

    /// Upper bound on the number of retries after the first attempt for a
    /// transient discovery failure.
    ///
    /// Exposed (`pub`) so callers and tests can reason about the total
    /// worst-case probe count for a persistently-failing endpoint, which is
    /// `1 + MAX_TRANSIENT_RETRIES`.
    pub const MAX_TRANSIENT_RETRIES: u32 = 2;

    /// Fixed backoff applied before each retry of a transient discovery
    /// failure. Intentionally constant (no jitter) because discovery is an
    /// infrequent background refresh, not a fan-out request hot path.
    const RETRY_BACKOFF: Duration = Duration::from_millis(500);

    /// Classify a [`ProviderError`] as transient (worth retrying).
    ///
    /// Returns `true` for transport-level failures (`Http`, including TLS
    /// resets, connection resets, and timeouts) and `Api` responses with a
    /// server-side 5xx status code. Client-side 4xx errors, parse failures,
    /// and configuration errors are non-transient.
    fn is_transient(error: &ProviderError) -> bool {
        match error {
            // Redirect-policy violations (e.g. too many redirects, or a
            // cross-host redirect refused by the custom policy) are
            // deterministic: retrying just re-runs the same redirect chain.
            ProviderError::Http { redirect: true, .. } => false,
            ProviderError::Http { .. } => true,
            ProviderError::Api { status, .. } => (500..600).contains(status),
            _ => false,
        }
    }

    fn build_request(
        &self,
        provider: &ProviderConfig,
        discovery: &ProviderDiscoveryConfig,
        url: &Url,
    ) -> RequestBuilder {
        let mut request = self.http.get(url.clone());
        // Transport boundary (discovery bypasses `AuthHeaders`): the secret is
        // exposed only here to build the reqwest auth header(s).
        let api_key: &str = provider.api_key.expose_secret();
        request = match provider.auth_style {
            AuthStyle::Bearer => request.bearer_auth(api_key),
            AuthStyle::XApiKey => request.header("x-api-key", api_key),
            AuthStyle::XGoogleApiKey => request.header("x-goog-api-key", api_key),
            AuthStyle::Both => request.bearer_auth(api_key).header("x-api-key", api_key),
        };
        if discovery.kind == ProviderDiscoveryKind::AnthropicModels
            && !discovery
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case(ANTHROPIC_VERSION_HEADER))
        {
            request = request.header(ANTHROPIC_VERSION_HEADER, DEFAULT_ANTHROPIC_VERSION);
        }
        for (name, value) in &discovery.headers {
            request = request.header(name, value);
        }
        request
    }

    /// Execute a single discovery request attempt: send, validate status, and
    /// stream the body up to `max_response_bytes`, then parse as JSON.
    async fn send_once(
        &self,
        request: RequestBuilder,
        max_response_bytes: usize,
    ) -> Result<Value, ProviderError> {
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            // Stream only a small prefix of the error body for diagnostics,
            // mirroring the success path's bounded read, so a misbehaving
            // discovery endpoint returning a large error body cannot spike
            // transient memory (audit discovery-error-body-full-read).
            let body_prefix =
                crate::error::read_error_body_bounded(response, MAX_DISCOVERY_ERROR_BODY_BYTES)
                    .await;
            return Err(ProviderError::api(status.as_u16(), body_prefix));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.try_next().await? {
            if bytes.len() + chunk.len() > max_response_bytes {
                return Err(ProviderError::InvalidConfig(format!(
                    "discovery response exceeded {} bytes",
                    max_response_bytes
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(ProviderError::from)
    }
}

fn parse_models(
    kind: ProviderDiscoveryKind,
    value: &Value,
    output: &mut Vec<StaticModelCatalogEntry>,
) -> Result<usize, ProviderError> {
    let records = match kind {
        ProviderDiscoveryKind::GeminiModels => value.get("models"),
        ProviderDiscoveryKind::FireworksAccountModels => {
            value.get("models").or_else(|| value.get("data"))
        }
        _ => value.get("data"),
    }
    .and_then(Value::as_array);

    let Some(records) = records else {
        return Err(ProviderError::InvalidConfig(
            "discovery response has an invalid top-level model list".to_owned(),
        ));
    };
    // Count of records dropped this page (no id / empty id, or a Gemini model
    // lacking generateContent support). Surfaced as an aggregate by `discover`
    // in the success path so a silent partial catalog is observable (GAP-LOW-6).
    let mut dropped = 0usize;
    for record in records {
        let id = record
            .get("id")
            .or_else(|| record.get("name"))
            .and_then(Value::as_str);
        let Some(id) = id.filter(|id| !id.trim().is_empty()) else {
            dropped += 1;
            tracing::warn!("ignoring malformed discovery model record without an id");
            continue;
        };
        if kind == ProviderDiscoveryKind::GeminiModels {
            let supports_generate = record
                .get("supportedGenerationMethods")
                .or_else(|| record.get("supportedActions"))
                .and_then(Value::as_array)
                .is_some_and(|actions| actions.iter().any(|action| action == "generateContent"));
            if !supports_generate {
                dropped += 1;
                tracing::debug!(
                    model = id,
                    "ignoring Gemini model without generateContent support"
                );
                continue;
            }
        }
        output.push(StaticModelCatalogEntry {
            id: id.to_owned(),
            display_name: record
                .get("display_name")
                .or_else(|| record.get("displayName"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            supports: vec![
                ProviderRouteKind::ChatCompletions,
                ProviderRouteKind::Messages,
            ],
            context_length: record
                .get("context_length")
                .or_else(|| record.get("inputTokenLimit"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
        });
    }
    Ok(dropped)
}

fn set_query_parameter(url: &mut Url, name: &str, value: &str) {
    // Single-pass query rebuild (audit GAP-LOW-5).
    //
    // Snapshot the existing query string once (a single allocation of the whole
    // query), clear it, then re-append every pair except the one named `name`,
    // and finally append the new value. `form_urlencoded::parse` yields
    // `Cow<str>` borrowed from the snapshot, so no per-pair `into_owned()` is
    // needed -- the prior implementation allocated two owned `String`s per pair
    // (one for the key, one for the value) solely to detach them from `url`'s
    // read borrow before the mutable `query_pairs_mut` borrow began. Parsing the
    // snapshot sidesteps that borrow conflict with a single allocation.
    //
    // All existing pairs keyed `name` are dropped and exactly one new pair is
    // appended, matching the prior behavior.
    let snapshot = url.query().unwrap_or("").to_owned();
    url.set_query(None);
    let mut serializer = url.query_pairs_mut();
    for (key, val) in form_urlencoded::parse(snapshot.as_bytes()) {
        if key != name {
            serializer.append_pair(&key, &val);
        }
    }
    serializer.append_pair(name, value);
}

fn next_page(kind: ProviderDiscoveryKind, value: &Value) -> Option<(&'static str, String)> {
    match kind {
        ProviderDiscoveryKind::GeminiModels => value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(|token| ("pageToken", token.to_owned())),
        ProviderDiscoveryKind::AnthropicModels => {
            let has_more = value
                .get("has_more")
                .and_then(Value::as_bool)
                .is_some_and(|b| b);
            if !has_more {
                return None;
            }
            let token = value.get("last_id").and_then(Value::as_str);
            if token.is_none() {
                tracing::warn!(
                    "Anthropic discovery: has_more is true but last_id is missing; \
                     pagination will stop early"
                );
            }
            token.map(|t| ("after_id", t.to_owned()))
        }
        ProviderDiscoveryKind::FireworksAccountModels => value
            .get("nextPageToken")
            .or_else(|| value.get("next_page_token"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(|token| ("pageToken", token.to_owned())),
        _ => None,
    }
}

impl Default for DiscoveryClient {
    /// Returns a default discovery client.
    ///
    /// # Lint expectation
    ///
    /// The `expect` below is deliberate: this is the infallible-in-practice
    /// convenience constructor (no custom TLS backend), with [`Self::try_new`]
    /// as the fallible alternative -- mirroring the convention established for
    /// `ProxyClient::new` in `transport.rs` (audit LOW-2). `#[expect]` makes the
    /// deliberate use compile-time-checked.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `reqwest::Client` cannot be constructed (e.g.
    /// due to a TLS backend initialization failure). Use [`DiscoveryClient::try_new`]
    /// for a fallible constructor.
    #[expect(
        clippy::expect_used,
        reason = "Default is infallible in practice; try_new is the fallible constructor"
    )]
    fn default() -> Self {
        Self::try_new().expect("default discovery HTTP client configuration is valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use llm_proxy_core::ProviderRoutesConfig;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    async fn echo_anthropic_version(headers: HeaderMap) -> Json<Value> {
        let version = headers
            .get(ANTHROPIC_VERSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing");
        Json(json!({
            "data": [{"id": version}],
            "has_more": false
        }))
    }

    async fn start_test_server(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}/models")
    }

    fn discovery_provider(
        endpoint: String,
        kind: ProviderDiscoveryKind,
        headers: HashMap<String, String>,
    ) -> ProviderConfig {
        ProviderConfig {
            name: "test-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::XApiKey,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: Some(ProviderDiscoveryConfig {
                kind,
                endpoint,
                headers,
                max_pages: 100,
                max_models: 20_000,
                max_response_bytes: 4 * 1024 * 1024,
            }),
            catalog: None,
            pricing: Default::default(),
        }
    }

    #[tokio::test]
    async fn anthropic_discovery_adds_default_version_header() {
        let endpoint =
            start_test_server(Router::new().route("/models", get(echo_anthropic_version))).await;
        let provider = discovery_provider(
            endpoint,
            ProviderDiscoveryKind::AnthropicModels,
            HashMap::new(),
        );

        let models = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect("Anthropic discovery should succeed");

        assert_eq!(models[0].id, DEFAULT_ANTHROPIC_VERSION);
    }

    #[tokio::test]
    async fn anthropic_discovery_preserves_configured_version_header() {
        let headers = HashMap::from([("Anthropic-Version".to_owned(), "2024-01-01".to_owned())]);
        let endpoint =
            start_test_server(Router::new().route("/models", get(echo_anthropic_version))).await;
        let provider =
            discovery_provider(endpoint, ProviderDiscoveryKind::AnthropicModels, headers);

        let models = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect("Anthropic discovery should succeed");

        assert_eq!(models[0].id, "2024-01-01");
    }

    #[tokio::test]
    async fn discovery_stops_at_configured_page_limit() {
        async fn paginated_models(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "models": [{
                    "name": "models/gemini-pro",
                    "supportedGenerationMethods": ["generateContent"]
                }],
                "nextPageToken": "next"
            }))
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let endpoint = start_test_server(
            Router::new()
                .route("/models", get(paginated_models))
                .with_state(Arc::clone(&calls)),
        )
        .await;
        let mut provider = discovery_provider(
            endpoint,
            ProviderDiscoveryKind::GeminiModels,
            HashMap::new(),
        );
        provider.discovery.as_mut().unwrap().max_pages = 1;

        let error = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect_err("page limit should stop pagination");

        assert!(error.to_string().contains("exceeded 1 pages"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn discovery_stops_at_configured_model_limit() {
        let endpoint = start_test_server(Router::new().route(
            "/models",
            get(|| async { Json(json!({"data": [{"id": "a"}, {"id": "b"}]})) }),
        ))
        .await;
        let mut provider = discovery_provider(
            endpoint,
            ProviderDiscoveryKind::OpenAiCompatibleModels,
            HashMap::new(),
        );
        provider.discovery.as_mut().unwrap().max_models = 1;

        let error = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect_err("model limit should stop discovery");

        assert!(error.to_string().contains("exceeded 1 models"));
    }

    #[tokio::test]
    async fn discovery_stops_at_configured_response_size_limit() {
        let endpoint = start_test_server(Router::new().route(
            "/models",
            get(|| async { Json(json!({"data": [{"id": "model"}]})) }),
        ))
        .await;
        let mut provider = discovery_provider(
            endpoint,
            ProviderDiscoveryKind::OpenAiCompatibleModels,
            HashMap::new(),
        );
        provider.discovery.as_mut().unwrap().max_response_bytes = 8;

        let error = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect_err("response size limit should stop discovery");

        assert!(error.to_string().contains("exceeded 8 bytes"));
    }

    #[tokio::test]
    async fn discovery_stops_on_non_advancing_pagination_token() {
        // A Gemini-style endpoint that always returns the SAME nextPageToken
        // must be detected: pagination stops early after the second identical
        // token instead of fetching the same page up to `max_pages` times
        // (audit discovery-nonadvancing-pagination-token).
        async fn static_token_models(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "models": [{
                    "name": "models/gemini-pro",
                    "supportedGenerationMethods": ["generateContent"]
                }],
                "nextPageToken": "stale"
            }))
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let endpoint = start_test_server(
            Router::new()
                .route("/models", get(static_token_models))
                .with_state(Arc::clone(&calls)),
        )
        .await;
        let provider = discovery_provider(
            endpoint,
            ProviderDiscoveryKind::GeminiModels,
            HashMap::new(),
        );

        let models = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect("non-advancing token should stop early, not error");

        // Exactly two pages fetched: the first yields the token, the second
        // returns the identical token and trips the non-advancing guard.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "pagination should stop after detecting the repeated token"
        );
        // The catalog gathered so far is valid and deduplicated.
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "models/gemini-pro");
    }

    #[test]
    fn openai_records_are_sorted_and_malformed_records_are_skipped() {
        let mut models = Vec::new();
        let dropped = parse_models(
            ProviderDiscoveryKind::OpenAiCompatibleModels,
            &json!({"data": [{"id": "z"}, {"missing": "id"}, {"id": "a"}]}),
            &mut models,
        )
        .expect("valid response");
        models.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        // GAP-LOW-6: the aggregate dropped-count must be surfaced from the
        // success path; the malformed `{"missing": "id"}` record is counted.
        assert_eq!(dropped, 1);
    }

    #[test]
    fn invalid_top_level_shape_fails() {
        let error = parse_models(
            ProviderDiscoveryKind::OpenAiModels,
            &json!({"models": []}),
            &mut Vec::new(),
        )
        .expect_err("invalid shape");
        assert!(error.to_string().contains("top-level"));
    }

    #[test]
    fn gemini_filters_non_generation_models() {
        let mut models = Vec::new();
        parse_models(
            ProviderDiscoveryKind::GeminiModels,
            &json!({"models": [
                {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]},
                {"name": "models/generate", "supportedGenerationMethods": ["generateContent"]}
            ]}),
            &mut models,
        )
        .expect("valid response");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "models/generate");
    }

    #[test]
    fn pagination_parameter_is_replaced() {
        let mut url = Url::parse("https://example.com/models?pageToken=old&limit=10").unwrap();
        set_query_parameter(&mut url, "pageToken", "new");
        assert_eq!(
            url.as_str(),
            "https://example.com/models?limit=10&pageToken=new"
        );
    }

    // -- discovery auth: secret is used on the wire AND redacted from Debug --

    /// Build a discovery provider with an explicit api key and auth style, so
    /// tests can prove each auth style places the exposed secret on the wire.
    fn discovery_provider_with_auth(
        endpoint: String,
        auth_style: AuthStyle,
        api_key: &str,
    ) -> ProviderConfig {
        ProviderConfig {
            name: "test-provider".to_owned(),
            api_key: secrecy::SecretString::from(api_key),
            auth_style,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: Some(ProviderDiscoveryConfig {
                kind: ProviderDiscoveryKind::OpenAiCompatibleModels,
                endpoint,
                headers: HashMap::new(),
                max_pages: 100,
                max_models: 20_000,
                max_response_bytes: 4 * 1024 * 1024,
            }),
            catalog: None,
            pricing: Default::default(),
        }
    }

    /// Test-server handler that echoes the received auth header value back as
    /// the single model id, so the test can assert which credential reached the
    /// wire. Configured via `EchoAuthConfig` state (the header name to read and
    /// an optional prefix to strip, e.g. the `Bearer ` scheme prefix).
    #[derive(Clone)]
    struct EchoAuthConfig {
        header_name: String,
        strip_prefix: Option<String>,
    }

    async fn echo_auth_header(
        State(config): State<Arc<EchoAuthConfig>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        let value = headers
            .get(config.header_name.as_str())
            .and_then(|v| v.to_str().ok())
            .unwrap_or("missing");
        let value = match &config.strip_prefix {
            Some(prefix) => value.strip_prefix(prefix).unwrap_or(value),
            None => value,
        };
        Json(json!({ "data": [{ "id": value }] }))
    }

    /// The discovery auth path exposes the secret ONLY to build the reqwest
    /// auth header(s); each auth style must place the configured key on the
    /// wire (proving the secret IS used), while the ProviderConfig Debug output
    /// must never contain the raw key (proving redaction). Parameterized over
    /// the three auth styles the discovery builder supports.
    async fn run_discovery_auth_on_the_wire_and_redacted(
        auth_style: AuthStyle,
        header_name: &'static str,
        strip_prefix: Option<&'static str>,
    ) {
        const SECRET: &str = "sk-discovery-secret-xyz";

        let config = Arc::new(EchoAuthConfig {
            header_name: header_name.to_owned(),
            strip_prefix: strip_prefix.map(str::to_owned),
        });
        let endpoint = start_test_server(
            Router::new()
                .route("/models", get(echo_auth_header))
                .with_state(config),
        )
        .await;
        let provider = discovery_provider_with_auth(endpoint, auth_style, SECRET);

        // (a) The exposed secret must reach the wire: the echo handler returns
        // the received auth-header value as the model id, which must equal the
        // configured key.
        let models = DiscoveryClient::default()
            .discover(&provider)
            .await
            .expect("discovery should succeed");
        assert_eq!(
            models.len(),
            1,
            "echo handler should return exactly one model"
        );
        assert_eq!(
            models[0].id, SECRET,
            "the configured api key must be sent on the wire as the auth header"
        );

        // (b) The ProviderConfig Debug output must NOT leak the raw key.
        let debug = format!("{provider:?}");
        assert!(
            !debug.contains(SECRET),
            "ProviderConfig Debug must redact the api key; leaked in: {debug}"
        );
        assert!(
            debug.contains("[REDACTED]"),
            "ProviderConfig Debug should mark the api key as redacted; got: {debug}"
        );
    }

    #[tokio::test]
    async fn discovery_bearer_auth_uses_secret_on_wire_and_redacts_debug() {
        run_discovery_auth_on_the_wire_and_redacted(
            AuthStyle::Bearer,
            "authorization",
            Some("Bearer "),
        )
        .await;
    }

    #[tokio::test]
    async fn discovery_x_api_key_auth_uses_secret_on_wire_and_redacts_debug() {
        run_discovery_auth_on_the_wire_and_redacted(AuthStyle::XApiKey, "x-api-key", None).await;
    }

    #[tokio::test]
    async fn discovery_x_goog_api_key_auth_uses_secret_on_wire_and_redacts_debug() {
        run_discovery_auth_on_the_wire_and_redacted(
            AuthStyle::XGoogleApiKey,
            "x-goog-api-key",
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn discovery_both_auth_uses_secret_on_wire_and_redacts_debug() {
        // AuthStyle::Both sets Bearer + x-api-key; assert the Bearer side lands
        // on the wire (the x-api-key side is covered by the XApiKey test).
        run_discovery_auth_on_the_wire_and_redacted(
            AuthStyle::Both,
            "authorization",
            Some("Bearer "),
        )
        .await;
    }
}
