use std::collections::HashMap;
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

/// Application state shared with every handler.
///
/// Cheap to clone (`Arc`-shared fields); the manual `Debug` impl never emits
/// secrets (regression-tested by `app_state_debug_does_not_leak_api_key`).
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    pub(crate) app_config: Arc<AppConfig>,
    pub(crate) providers: Arc<ProviderRegistry>,
    pub(crate) provider_adapters: Arc<ProviderAdapterRegistry>,
    pub(crate) proxy_client: Arc<ProxyClient>,
    pub(crate) egress_clients: Arc<HashMap<String, Arc<ProxyClient>>>,
    pub(crate) build: Arc<BuildInfo>,
    pub(crate) token_counter: Counter,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) rate_limiter: Arc<RateLimiter>,
    pub(crate) request_dedup: Arc<RequestDeduplicator>,
    pub(crate) request_id_gen: Arc<RequestIdGenerator>,
    pub(crate) model_catalogs: Arc<ModelCatalogService>,
    pub(crate) event_bus: Arc<dyn EventBus>,
    pub(crate) client_auth: Arc<ClientAuth>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("app_config", &self.app_config)
            .field("providers", &self.providers)
            .field("provider_adapters", &self.provider_adapters)
            .field("proxy_client", &self.proxy_client)
            .field("egress_clients", &self.egress_clients)
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
        let dedup_window_ms =
            u64::try_from(app_config.server.dedup_window.as_millis()).unwrap_or(0);
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
            egress_clients: Arc::new(HashMap::new()),
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

    /// Replace the client-auth state (builder-style).
    #[must_use]
    pub fn with_client_auth(mut self, client_auth: Arc<ClientAuth>) -> Self {
        self.client_auth = client_auth;
        self
    }

    /// Replace the per-provider egress clients (builder-style).
    ///
    /// An empty map (the default) keeps every provider on the shared
    /// `proxy_client`.
    #[must_use]
    pub fn with_egress_clients(
        mut self,
        egress_clients: HashMap<String, Arc<ProxyClient>>,
    ) -> Self {
        self.egress_clients = Arc::new(egress_clients);
        self
    }

    /// Replace the event bus (builder-style).
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

    /// Configured CORS allow-origin list, if any. `None` permits no
    /// cross-origin requests (no `CorsLayer` installed); `Some(origins)` are
    /// the exact origins the `CorsLayer` echoes.
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

    /// The transport client for an upstream request to `provider`: its wired
    /// `[provider.egress]` client, or the shared default client otherwise.
    /// An egress-configured-but-unwired provider logs a loud warning per request.
    pub fn upstream_client_for(&self, provider: &str) -> &ProxyClient {
        if let Some(client) = self.egress_clients.get(provider) {
            return client;
        }
        if self
            .providers
            .get(provider)
            .is_some_and(|config| config.egress.is_some())
        {
            tracing::warn!(
                provider,
                "provider has [provider.egress] configured but no egress client was wired; \
                 its upstream traffic is leaving directly, not through the configured proxy"
            );
        }
        &self.proxy_client
    }

    /// Access the provider model catalog service.
    pub fn model_catalogs(&self) -> &ModelCatalogService {
        &self.model_catalogs
    }

    /// Access the request/response event sink.
    ///
    /// Holds the bus so the serve path can [`EventBus::close`] it (flush the
    /// usage ledger) after the HTTP server has stopped.
    pub fn event_bus(&self) -> &Arc<dyn EventBus> {
        &self.event_bus
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
    use std::net::SocketAddr;

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
                admin_bind: None,
                admin_on_main_listener: false,
            },
            auth: Default::default(),
            storage: None,
        }
    }

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

    #[test]
    fn upstream_client_for_returns_the_shared_client_without_egress() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        for provider in ["opencode-go", "opencode-zen", "anything-else"] {
            assert!(
                std::ptr::eq(
                    state.upstream_client_for(provider),
                    state.proxy_client.as_ref()
                ),
                "provider {provider} without egress must use the shared client"
            );
        }
    }

    #[test]
    fn upstream_client_for_returns_the_wired_client_only_for_that_provider() {
        let egress_client = Arc::new(
            ProxyClient::try_with_egress_proxy("socks5h://127.0.0.1:1080")
                .expect("egress proxy client builds"),
        );
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        )
        .with_egress_clients(HashMap::from([(
            "opencode-go".to_owned(),
            Arc::clone(&egress_client),
        )]));

        assert!(
            std::ptr::eq(
                state.upstream_client_for("opencode-go"),
                egress_client.as_ref()
            ),
            "the provider with egress must use its wired client"
        );
        assert!(
            std::ptr::eq(
                state.upstream_client_for("opencode-zen"),
                state.proxy_client.as_ref()
            ),
            "providers without egress must keep the shared client"
        );
    }

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

    #[test]
    fn app_state_debug_does_not_leak_api_key() {
        use llm_proxy_core::{AuthStyle, ProviderConfig, ProviderRoutesConfig};
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: secrecy::SecretString::from("sk-secret-key-99999"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
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
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        let debug_output = format!("{:?}", state);
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

    #[test]
    fn provider_adapters_are_populated() {
        let state = AppState::new(
            make_app_config(),
            make_provider_registry(),
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            make_build_info(),
        );
        let protocol_names = state.provider_adapters.protocol_names();
        assert!(
            protocol_names.len() >= 4,
            "expected at least 4 builtin adapters, got {len}",
            len = protocol_names.len()
        );
        assert!(
            protocol_names.contains(&"openai_chat_completions"),
            "missing openai_chat_completions adapter"
        );
        assert!(
            protocol_names.contains(&"anthropic_messages"),
            "missing anthropic_messages adapter"
        );
    }

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
            egress: None,
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
            egress: None,
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

    #[test]
    fn config_extension_dispatch_classifies_correctly() {
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

    #[test]
    fn app_state_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<AppState>();
    }

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
    }

    #[test]
    fn multi_provider_toml_mixed_protocols() {
        use llm_proxy_core::{
            AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRoutesConfig,
        };

        let good = ProviderConfig {
            name: "good".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
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

        let bad = ProviderConfig {
            name: "bad".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
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

    #[test]
    fn zero_duration_timeout_is_returned() {
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

    #[test]
    fn invalid_auth_config_fails_closed() {
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
