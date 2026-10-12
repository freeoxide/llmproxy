use std::sync::Arc;
use std::time::Duration;

use llm_proxy_core::{AppConfig, Counter, Metrics, ProviderRegistry};
use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
use llm_proxy_storage::{EventBus, NoopBus};

use crate::ModelCatalogService;
use crate::middleware::{ClientAuth, RateLimiter, RequestDeduplicator, RequestIdGenerator};

/// Static information about this binary.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Package name.
    pub name: &'static str,
    /// Package version.
    pub version: &'static str,
    /// Target triple the binary was compiled for.
    pub target: &'static str,
    /// Git commit SHA the binary was built from.
    pub git_sha: &'static str,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Application state shared with every handler.
///
/// Cheap to clone: nearly all fields are `Arc`-wrapped so cloning only bumps
/// reference counts. `token_counter` is a non-ZST `Counter` stored inline, but
/// its lazily-loaded BPE cache is held in an inner `Arc`, so cloning `Counter`
/// is a refcount bump and every `AppState` clone shares the same loaded BPE
/// encodings. Required by axum's `State` extractor.
///
/// # Security note
///
/// Manual `Debug` impl is provided to ensure `api_key` in nested config types
/// is never leaked through debug formatting (e.g. in error logs).
///
/// The transitive redaction chain for Debug is:
/// - `AppState` -> `app_config` field: `AppConfig` has no secrets (safe).
/// - `AppState` -> `providers` field: `ProviderRegistry` -> `ProviderConfig`,
///   whose `api_key` is a `secrecy::SecretString`; `ProviderConfig` derives
///   `Debug` and the key is redacted by `SecretString`'s own impl. Safe.
/// - `AppState` -> `provider_adapters` field: `ProviderAdapterRegistry` ->
///   `ProviderAdapter` types whose `api_key` is `SecretString`; the adapter
///   config/target types keep a *manual* `Debug` that additionally redacts the
///   endpoint query string and auth-header *values* (not just the key). Safe.
/// - `AppState` -> `client_auth` field: `ClientAuth` holds **hashed argon2id
///   verifiers only, never plaintext client secrets**; its manual `Debug`
///   prints the mode and key ids and never the verifier strings (a PHC hash
///   is offline-crackable, so it is treated as sensitive). Safe.
/// - `AppState` -> remaining fields: `BuildInfo`, `proxy_client`, `Counter`,
///   `Metrics`, `RateLimiter`, `RequestDeduplicator`, `RequestIdGenerator`,
///   `model_catalogs`, `event_bus` -- none hold secrets. Safe.
///
/// **Maintenance note:** Secret-backed fields use `secrecy::SecretString`, which
/// redacts in `Debug`/`Display` regardless of whether the containing type
/// derives or hand-writes `Debug`. The remaining risk is non-`SecretString`
/// sensitive data (e.g. endpoint query strings, raw header values): those still
/// rely on the manual `Debug` impls on the adapter config/target types. If such
/// a field is added to a type that derives `Debug`, audit it. The test
/// `app_state_debug_does_not_leak_api_key` provides regression coverage.
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    /// TOML application config.
    pub(crate) app_config: Arc<AppConfig>,
    /// Provider registry loaded from TOML provider files.
    pub(crate) providers: Arc<ProviderRegistry>,
    /// Provider adapter registry (always present).
    pub(crate) provider_adapters: Arc<ProviderAdapterRegistry>,
    /// Protocol-neutral HTTP transport client.
    pub(crate) proxy_client: Arc<ProxyClient>,
    /// Build identifier reported by `/version`.
    pub(crate) build: Arc<BuildInfo>,
    /// Token counter for estimating token usage.
    ///
    /// `Counter` dispatches to a real BPE tokenizer (tiktoken) for known OpenAI
    /// models and a character heuristic otherwise. It holds a lazily-populated,
    /// `Arc`-shared cache of loaded BPE encodings behind a `Mutex`, so cloning
    /// the counter via `AppState: Clone` is a cheap refcount bump that shares
    /// the cache across handlers.
    pub(crate) token_counter: Counter,
    /// Runtime metrics collector.
    pub(crate) metrics: Arc<Metrics>,
    /// Per-IP rate limiter.
    pub(crate) rate_limiter: Arc<RateLimiter>,
    /// Request deduplicator for idempotent request handling.
    pub(crate) request_dedup: Arc<RequestDeduplicator>,
    /// Request ID generator.
    pub(crate) request_id_gen: Arc<RequestIdGenerator>,
    /// Mutable provider model catalog cache and discovery coordinator.
    pub(crate) model_catalogs: Arc<ModelCatalogService>,
    /// Structured request/response event sink. Defaults to [`NoopBus`]; a real
    /// persisted backend lands in a later phase.
    pub(crate) event_bus: Arc<dyn EventBus>,
    /// Client-key authentication state: hashed argon2id verifiers plus a
    /// digest cache. Built from `[auth]` config; construction from an invalid
    /// auth config fails closed (enforced, nothing can authenticate) rather
    /// than silently downgrading to no-auth, because the constructors are
    /// infallible. `cmd_serve` surfaces invalid verifiers as a startup error
    /// via `ClientAuth::from_config` before the state is built.
    pub(crate) client_auth: Arc<ClientAuth>,
}

impl std::fmt::Debug for AppState {
    // SECURITY: This manual Debug impl delegates through Arc to each field's
    // Debug. The redaction chain documented on the struct must be maintained.
    // The test `app_state_debug_does_not_leak_api_key` provides regression
    // coverage. See struct-level doc comment for the full chain.
    //
    // NOTE: A `redacted_debug!()` macro could reduce boilerplate for types
    // with sensitive fields, but would add a procedural-macro dependency.
    // The manual impl is preferred for now because: (a) AppState has few
    // fields, (b) the redaction chain is explicitly documented and tested,
    // and (c) a macro would obscure the security-critical delegation paths.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("app_config", &self.app_config)
            .field("providers", &self.providers)
            .field("provider_adapters", &self.provider_adapters)
            .field("proxy_client", &self.proxy_client)
            .field("build", &self.build)
            .field("token_counter", &self.token_counter)
            .field("metrics", &self.metrics)
            .field("rate_limiter", &self.rate_limiter)
            .field("request_dedup", &self.request_dedup)
            .field("request_id_gen", &self.request_id_gen)
            .field("model_catalogs", &self.model_catalogs)
            .field("event_bus", &self.event_bus)
            .field("client_auth", &self.client_auth)
            .finish()
    }
}

impl AppState {
    /// Construct `AppState` with TOML configuration.
    #[must_use]
    pub fn new(
        app_config: AppConfig,
        providers: ProviderRegistry,
        provider_adapters: ProviderAdapterRegistry,
        proxy_client: ProxyClient,
        build: BuildInfo,
    ) -> Self {
        Self::new_with_catalog_dir(
            app_config,
            providers,
            provider_adapters,
            proxy_client,
            build,
            None,
        )
    }

    /// Construct `AppState` with an optional on-disk provider catalog directory.
    #[must_use]
    pub fn new_with_catalog_dir(
        app_config: AppConfig,
        providers: ProviderRegistry,
        provider_adapters: ProviderAdapterRegistry,
        proxy_client: ProxyClient,
        build: BuildInfo,
        catalog_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let rate_limit_rpm = app_config.server.rate_limit_rpm;
        // A dedup window whose millis do not fit u64 (Duration::MAX) cannot
        // meaningfully be represented; fall back to 0, which disables dedup,
        // matching the documented "0s disables deduplication" contract.
        let dedup_window_ms =
            u64::try_from(app_config.server.dedup_window.as_millis()).unwrap_or(0);
        // Auth state from `[auth]` config. Construction is infallible, so an
        // invalid auth config (e.g. an unparseable verifier) fails CLOSED --
        // enforced with zero verifiers, rejecting every proxied request -- and
        // logs the error, rather than silently downgrading to no-auth. The
        // serve path validates the auth config first (`ClientAuth::from_config`
        // in `load_toml_state`) and turns this into a startup error.
        let client_auth = match ClientAuth::from_config(&app_config.auth) {
            Ok(auth) => Arc::new(auth),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "invalid [auth] config; failing closed (all proxy requests will be rejected)"
                );
                Arc::new(ClientAuth::fail_closed())
            }
        };
        Self {
            app_config: Arc::new(app_config),
            providers: Arc::new(providers),
            provider_adapters: Arc::new(provider_adapters),
            proxy_client: Arc::new(proxy_client),
            build: Arc::new(build),
            token_counter: Counter::new(),
            metrics: Arc::new(Metrics::new()),
            rate_limiter: Arc::new(RateLimiter::new(rate_limit_rpm)),
            request_dedup: Arc::new(RequestDeduplicator::with_window_ms(dedup_window_ms)),
            request_id_gen: Arc::new(RequestIdGenerator::new()),
            model_catalogs: Arc::new(ModelCatalogService::new(catalog_dir)),
            event_bus: Arc::new(NoopBus) as Arc<dyn EventBus>,
            client_auth,
        }
    }

    /// Replace the client-auth state (builder-style). Used by `cmd_serve`
    /// (after `ClientAuth::from_config` succeeds, so an invalid verifier is a
    /// startup error rather than a runtime fail-closed) and by tests that
    /// exercise authenticated routing.
    #[must_use]
    pub fn with_client_auth(mut self, client_auth: Arc<ClientAuth>) -> Self {
        self.client_auth = client_auth;
        self
    }

    /// Replace the event bus (builder-style). Used by tests to inject a
    /// [`RecordingBus`](llm_proxy_storage::RecordingBus); production wires the
    /// real bus in `cmd_serve` (still `NoopBus` for now).
    #[must_use]
    pub fn with_event_bus(mut self, event_bus: Arc<dyn EventBus>) -> Self {
        self.event_bus = event_bus;
        self
    }

    /// Request timeout duration from app config.
    pub fn request_timeout(&self) -> Duration {
        self.app_config.server.request_timeout
    }

    /// Graceful-shutdown hard deadline from app config.
    ///
    /// `Duration::ZERO` means "no hard deadline" (drain indefinitely); every
    /// other value bounds how long in-flight connections may delay shutdown.
    pub fn shutdown_timeout(&self) -> Duration {
        self.app_config.server.shutdown_timeout
    }

    /// Server name for responses and version endpoints.
    pub fn server_name(&self) -> &str {
        &self.app_config.server.server_name
    }

    /// Whether trusted reverse-proxy headers may determine client identity.
    pub fn trust_forwarded_headers(&self) -> bool {
        self.app_config.server.trust_forwarded_headers
    }

    /// Configured CORS allow-origin list, if any (audit LOW-12).
    ///
    /// `None` means cross-origin requests are not permitted: no `CorsLayer` is
    /// installed and browsers enforce a same-origin default. `Some(origins)`
    /// are the exact origins a restrictive `CorsLayer` will echo.
    pub fn allowed_origins(&self) -> Option<&[String]> {
        self.app_config.server.allowed_origins.as_deref()
    }

    /// Access the TOML app config.
    pub fn app_config(&self) -> &AppConfig {
        &self.app_config
    }

    /// Access the provider registry.
    pub fn providers(&self) -> &ProviderRegistry {
        &self.providers
    }

    /// Access the provider model catalog service.
    pub fn model_catalogs(&self) -> &ModelCatalogService {
        &self.model_catalogs
    }

    /// Return the bind address the server should listen on.
    pub fn bind_address(&self) -> std::net::SocketAddr {
        self.app_config.server.bind
    }

    /// Access build metadata (version, target, git SHA).
    pub fn build_info(&self) -> &BuildInfo {
        &self.build
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;

    /// Helper: build a minimal TOML AppConfig for tests.
    fn make_app_config() -> AppConfig {
        use llm_proxy_core::ServerConfig;
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
        }
    }

    /// Helper: build a minimal ProviderRegistry for tests.
    fn make_provider_registry() -> ProviderRegistry {
        ProviderRegistry::from_providers(vec![]).expect("empty registry")
    }

    fn make_build_info() -> BuildInfo {
        BuildInfo {
            name: "test",
            version: "0.0.0",
            target: "test",
            git_sha: "test",
        }
    }

    // -- AppState construction ------------------------------------------------

    #[test]
    fn app_state_construction() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.server_name(), "test-proxy");
        assert_eq!(state.request_timeout(), Duration::from_secs(300));
        assert_eq!(state.shutdown_timeout(), Duration::from_secs(30));
    }

    // -- Helper methods -------------------------------------------------------

    #[test]
    fn helpers_return_app_config_values() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.server_name(), "test-proxy");
        assert_eq!(state.request_timeout(), Duration::from_secs(300));
    }

    // -- Debug does not leak API keys -----------------------------------------

    #[test]
    fn app_state_debug_does_not_leak_api_key() {
        use llm_proxy_core::{AuthStyle, ProviderConfig, ProviderRoutesConfig};
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: secrecy::SecretString::from("sk-secret-key-99999"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let state = AppState::new(
            make_app_config(),
            registry,
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        let debug_output = format!("{:?}", state);
        assert!(
            !debug_output.contains("sk-secret-key-99999"),
            "Debug output must not contain the actual api_key, got: {debug_output}"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must show [REDACTED] for api_key, got: {debug_output}"
        );
    }

    #[test]
    fn app_state_debug_covers_all_fields() {
        // Regression test: verify that the Debug impl actually outputs all
        // field names. If a field is added to AppState but missing from the
        // manual Debug impl, this test catches it.
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        let debug_output = format!("{:?}", state);
        // All fields that should appear in the Debug output.
        for field in [
            "app_config",
            "providers",
            "provider_adapters",
            "proxy_client",
            "build",
            "token_counter",
            "metrics",
            "rate_limiter",
            "request_dedup",
            "request_id_gen",
            "model_catalogs",
            "event_bus",
            "client_auth",
        ] {
            assert!(
                debug_output.contains(field),
                "Debug output must contain field '{field}', got: {debug_output}"
            );
        }
    }

    // -- Provider adapters are populated --------------------------------------

    #[test]
    fn provider_adapters_are_populated() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        // Use >= 4 rather than == 4 so adding new builtin adapters does not
        // break this test (the intent is to verify adapters are populated).
        let protocol_names = state.provider_adapters.protocol_names();
        assert!(
            protocol_names.len() >= 4,
            "expected at least 4 builtin adapters, got {len}",
            len = protocol_names.len()
        );
        // Verify specific known protocol names are present for stronger coverage.
        assert!(
            protocol_names.contains(&"openai_chat_completions"),
            "missing openai_chat_completions adapter"
        );
        assert!(
            protocol_names.contains(&"anthropic_messages"),
            "missing anthropic_messages adapter"
        );
    }

    // -- TOML provider protocol validation against builtin -------------------

    #[test]
    fn toml_provider_protocol_validation_against_builtin() {
        use llm_proxy_core::{
            AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRoutesConfig,
        };
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: "https://example.com/v1".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: "https://example.com/v1/messages".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let adapter_reg = ProviderAdapterRegistry::builtin();
        let result = registry.validate_protocols(adapter_reg.protocol_names());
        assert!(result.is_ok(), "all protocols are builtin, got: {result:?}");
    }

    // -- TOML unknown protocol fails validation against builtin ---------------

    #[test]
    fn toml_unknown_protocol_fails_validation_against_builtin() {
        use llm_proxy_core::{
            AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRoutesConfig,
        };
        let provider = ProviderConfig {
            name: "bad".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "fake".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "not_a_real_protocol".to_owned(),
                        endpoint: "https://example.com".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let adapter_reg = ProviderAdapterRegistry::builtin();
        let result = registry.validate_protocols(adapter_reg.protocol_names());
        assert!(result.is_err(), "unknown protocol should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown protocol"),
            "expected unknown protocol error, got: {err}"
        );
    }

    // -- Config extension dispatch logic -------------------------------------

    #[test]
    fn config_extension_dispatch_classifies_correctly() {
        /// Mimics the match arm logic from cmd_serve to verify extension
        /// classification without requiring a full integration test.
        fn classify(path: &std::path::Path) -> &'static str {
            match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "toml" => "toml",
                "json" => "json",
                _ => "unsupported",
            }
        }

        assert_eq!(classify(std::path::Path::new("config.toml")), "toml");
        assert_eq!(classify(std::path::Path::new("config.json")), "json");
        assert_eq!(classify(std::path::Path::new("config.yaml")), "unsupported");
        assert_eq!(classify(std::path::Path::new("config.txt")), "unsupported");
        assert_eq!(classify(std::path::Path::new("config")), "unsupported");
        assert_eq!(classify(std::path::Path::new("config.YAML")), "unsupported");
    }

    // -- Send + Sync static assertion for AppState ---------------------------

    #[test]
    fn app_state_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<AppState>();
    }

    // -- bind_address() tests ------------------------------------------------

    #[test]
    fn bind_address_returns_configured_bind() {
        let mut cfg = make_app_config();
        cfg.server.bind = "10.0.0.1:8080".parse().unwrap();
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(
            state.bind_address(),
            "10.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
    }

    // -- Clone shares Arc references -----------------------------------------

    #[test]
    fn clone_shares_arc_references() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        let cloned = state.clone();
        assert!(
            Arc::ptr_eq(&state.metrics, &cloned.metrics),
            "cloned AppState should share the same Arc<Metrics>"
        );
        assert!(
            Arc::ptr_eq(&state.build, &cloned.build),
            "cloned AppState should share the same Arc<BuildInfo>"
        );
        // NOTE: `token_counter` is an inline `Counter` (not `Arc<Counter>`), so
        // there is no outer `Arc<Counter>` to compare with `Arc::ptr_eq`. It is
        // intentionally excluded from the Arc-sharing assertions above. The
        // loaded BPE cache is still shared: `Counter` holds its cache in an
        // inner `Arc<Mutex<HashMap<..>>>`, so this `cloned` state reuses the
        // same loaded encodings as the original.
    }

    // -- Multi-provider TOML validation --------------------------------------

    #[test]
    fn multi_provider_toml_mixed_protocols() {
        use llm_proxy_core::{
            AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRoutesConfig,
        };

        // Provider with valid protocols
        let good = ProviderConfig {
            name: "good".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: "https://example.com/v1".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };

        // Provider with invalid protocol
        let bad = ProviderConfig {
            name: "bad".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "fake".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "not_real".to_owned(),
                        endpoint: "https://example.com".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };

        let registry = ProviderRegistry::from_providers(vec![good, bad]).expect("registry");
        let adapter_reg = ProviderAdapterRegistry::builtin();
        let result = registry.validate_protocols(adapter_reg.protocol_names());
        assert!(
            result.is_err(),
            "mixed providers with one bad protocol should fail"
        );
    }

    // -- Boundary values for helpers -----------------------------------------

    #[test]
    fn zero_duration_timeout_is_returned() {
        // AppState is a dependency-injection container: request_timeout() is a
        // pure passthrough accessor and performs no validation. Zero/sub-second
        // timeouts are rejected earlier, at config load, in `load_app_config`
        // (see `llm-proxy-core` `validate_*` — HIGH-2 / LOW-10). This test
        // documents the accessor's faithfulness to whatever ServerConfig it is
        // handed, not an endorsement of `request_timeout = "0s"` as valid config.
        let mut cfg = make_app_config();
        cfg.server.request_timeout = Duration::ZERO;
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.request_timeout(), Duration::ZERO);
    }

    #[test]
    fn empty_server_name_is_returned() {
        let mut cfg = make_app_config();
        cfg.server.server_name = String::new();
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.server_name(), "");
    }

    // -- Unicode server_name ---------------------------------------------------

    #[test]
    fn unicode_server_name_is_returned_verbatim() {
        let mut cfg = make_app_config();
        cfg.server.server_name = "代理服务器-ünïcödé".to_owned();
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.server_name(), "代理服务器-ünïcödé");
    }

    #[test]
    fn very_long_server_name_is_returned() {
        let mut cfg = make_app_config();
        let long_name = "x".repeat(10_000);
        cfg.server.server_name = long_name.clone();
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert_eq!(state.server_name(), long_name);
    }

    // -- Client auth state ------------------------------------------------------

    #[test]
    fn invalid_auth_config_fails_closed() {
        // An unparseable verifier cannot be allowed to downgrade the server to
        // no-auth: the state must come up enforced with zero verifiers, so
        // every proxied request is rejected.
        let mut cfg = make_app_config();
        cfg.auth.mode = llm_proxy_core::AuthMode::ClientKeys;
        cfg.auth.clients.insert(
            "laptop".to_owned(),
            secrecy::SecretString::from("not-a-phc-string"),
        );
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        assert!(
            state.client_auth.is_enforced(),
            "invalid auth config must fail closed, not open"
        );
        assert_eq!(state.client_auth.client_count(), 0);
    }

    #[test]
    fn app_state_debug_does_not_leak_client_verifiers() {
        // The client verifier hash must never surface through Debug, matching
        // the api_key discipline. Use a real (cheap-parameter) argon2id hash
        // so the verifier parses.
        use argon2::{Algorithm, Argon2, CustomizedPasswordHasher, Params, Version};
        let params = Params::new(8, 1, 1, None).expect("valid test params");
        let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone());
        let phc = hasher
            .hash_password_customized(
                b"leakable-secret-tail",
                b"0123456789abcdef",
                Some(&argon2::ARGON2ID_IDENT),
                None,
                params,
            )
            .expect("hash token")
            .to_string();

        let mut cfg = make_app_config();
        cfg.auth.mode = llm_proxy_core::AuthMode::ClientKeys;
        cfg.auth
            .clients
            .insert("laptop".to_owned(), secrecy::SecretString::from(phc));
        let client_auth =
            crate::middleware::ClientAuth::from_config(&cfg.auth).expect("valid verifier");
        let state = AppState::new(
            cfg,
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        )
        .with_client_auth(Arc::new(client_auth));
        let debug = format!("{state:?}");
        assert!(
            !debug.contains("leakable-secret-tail"),
            "client secret/verifier must not appear in Debug, got: {debug}"
        );
        assert!(debug.contains("laptop"), "key id may appear: {debug}");
    }
}
