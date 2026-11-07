use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use llm_proxy_adapters::ProviderAdapterRegistry;
use llm_proxy_core::{AppConfig, Counter, Metrics, ProviderRegistry};
use llm_proxy_engine::{Engine, EngineBuilder, ModelCatalogService};
use llm_proxy_storage::{EventBus, NoopBus};
use llm_proxy_transport::ProxyClient;

use crate::middleware::{ClientAuth, RateLimiter, RequestIdGenerator};

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
    pub(crate) request_id_gen: Arc<RequestIdGenerator>,
    pub(crate) model_catalogs: Arc<ModelCatalogService>,
    pub(crate) event_bus: Arc<dyn EventBus>,
    pub(crate) client_auth: Arc<ClientAuth>,
    /// The configuration snapshots the engine reads per pipeline stage (plan
    /// 07 step 10): swapping the whole immutable snapshot is the one update
    /// path; nothing merges into a live one.
    pub(crate) config_source: Arc<llm_proxy_engine::SwappingConfigSource>,
    /// A replica-shared admission step (plan 09 step 1), injected by the
    /// daemon when `[replica]` is configured: it replaces the in-process
    /// one wholesale — the rules are the same, the counters are shared.
    pub(crate) admission_override: Option<Arc<dyn llm_proxy_engine::Admission>>,
    /// Replica-shared cooldowns (plan 09 step 1): one replica's cooldown is
    /// every replica's.
    pub(crate) cooldowns_override: Option<Arc<dyn llm_proxy_engine::Cooldowns>>,
    /// Shared by every clone of this state: axum hands each request its own
    /// `AppState` clone, and the engine carries process-wide state (response
    /// cache, single-flight, idempotency, cooldowns), so a per-clone cell
    /// would give every request a fresh, empty engine. The slot itself can be
    /// swapped by a hot reload (plan 07 step 11); requests already holding an
    /// engine clone finish on it.
    engine: Arc<EngineSlot>,
}

/// The one engine slot, lazily built once and hot-swappable afterwards.
#[derive(Debug, Default)]
struct EngineSlot {
    built: std::sync::RwLock<Option<Result<Arc<Engine>, llm_proxy_engine::EngineError>>>,
}

impl EngineSlot {
    fn get(&self) -> Option<Result<Arc<Engine>, llm_proxy_engine::EngineError>> {
        let guard = self
            .built
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }

    fn set(&self, built: Result<Arc<Engine>, llm_proxy_engine::EngineError>) {
        let mut guard = self
            .built
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(built);
    }

    fn replace(
        &self,
        engine: Arc<Engine>,
    ) -> Option<Result<Arc<Engine>, llm_proxy_engine::EngineError>> {
        let mut guard = self
            .built
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.replace(Ok(engine))
    }
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
            .field("request_id_gen", &self.request_id_gen)
            .field("model_catalogs", &self.model_catalogs)
            .field("event_bus", &self.event_bus)
            .field("client_auth", &self.client_auth)
            .field(
                "engine",
                &self
                    .engine
                    .get()
                    .map(|result| result.as_ref().map_or("<build failed>", |_| "<engine>"))
                    .unwrap_or("<not materialized>"),
            )
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
        let app_config = Arc::new(app_config);
        let config_source = Arc::new(llm_proxy_engine::SwappingConfigSource::new(Arc::clone(
            &app_config,
        )));
        Self {
            app_config,
            providers: Arc::new(providers),
            provider_adapters: Arc::new(provider_adapters),
            proxy_client: Arc::new(proxy_client),
            egress_clients: Arc::new(HashMap::new()),
            build: Arc::new(build),
            token_counter: Counter::new(),
            metrics: Arc::new(Metrics::new()),
            rate_limiter: Arc::new(RateLimiter::new(rate_limit_rpm)),
            request_id_gen: Arc::new(RequestIdGenerator::new()),
            model_catalogs: Arc::new(ModelCatalogService::new(catalog_dir)),
            event_bus: Arc::new(NoopBus) as Arc<dyn EventBus>,
            client_auth,
            config_source,
            admission_override: None,
            cooldowns_override: None,
            engine: Arc::new(EngineSlot::default()),
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

    /// Replace the admission step with a replica-shared one (plan 09 step
    /// 1): the daemon injects this when `[replica]` is configured, so the
    /// same rules run over shared counters instead of this process's.
    #[must_use]
    pub fn with_admission(mut self, admission: Arc<dyn llm_proxy_engine::Admission>) -> Self {
        self.admission_override = Some(admission);
        self
    }

    /// Replace the cooldown view with a replica-shared one (plan 09 step
    /// 1).
    #[must_use]
    pub fn with_cooldowns(mut self, cooldowns: Arc<dyn llm_proxy_engine::Cooldowns>) -> Self {
        self.cooldowns_override = Some(cooldowns);
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

    /// The live provider registry: the current engine's after a hot reload
    /// swapped it (plan 07 step 11), the startup registry before the first
    /// build. An owned `Arc`, because the answer can change between calls.
    #[must_use]
    pub fn providers(&self) -> Arc<ProviderRegistry> {
        match self.engine.get() {
            Some(Ok(engine)) => engine.provider_registry(),
            _ => Arc::clone(&self.providers),
        }
    }

    /// The request engine: the shared lifecycle (admission, routing,
    /// credentials, egress, settlement). Materialized once, on first use,
    /// from this state's config — the daemon's config loader validates
    /// provider protocols at startup, while an embedder assembling its own
    /// stack materializes lazily and sees build errors per request.
    ///
    /// # Errors
    ///
    /// [`llm_proxy_engine::EngineError`] when the provider set names a
    /// protocol no registered adapter serves (surfaced per request as a 500).
    pub fn engine(&self) -> Result<Arc<Engine>, llm_proxy_engine::EngineError> {
        if let Some(built) = self.engine.get() {
            return built;
        }
        let built = self.build_engine();
        self.engine.set(built.clone());
        built
    }

    /// The configuration snapshots the engine reads (plan 07 step 10): the
    /// startup snapshot until swapped, whole-snapshot swaps only.
    #[must_use]
    pub fn config_source(&self) -> &Arc<llm_proxy_engine::SwappingConfigSource> {
        &self.config_source
    }

    /// Builds a fresh engine from this state's current parts. Used for the
    /// lazy first build; the hot-reload path (plan 07 step 11) builds from
    /// explicit parts and then swaps the slot, so requests holding the
    /// previous engine finish on it because they hold an `Arc` clone.
    pub(crate) fn build_engine(&self) -> Result<Arc<Engine>, llm_proxy_engine::EngineError> {
        let egress = (*self.egress_clients).clone();
        for provider in self.providers.iter() {
            if provider.egress.is_some() && !egress.contains_key(&provider.name) {
                tracing::warn!(
                    provider = %provider.name,
                    "provider has [provider.egress] configured but no egress client was wired; \
                     its upstream traffic is leaving directly, not through the configured proxy"
                );
            }
        }
        self.build_engine_from_parts(&self.providers, egress)
    }

    /// Builds an engine over an explicit registry and egress map — the
    /// hot-reload path (plan 07 step 11): the new engine is built and
    /// validated before anything live is touched. Admission (per-key
    /// policies and concurrency caps) is rebuilt against the new registry,
    /// since caps live on providers.
    pub fn build_engine_from_parts(
        &self,
        providers: &Arc<ProviderRegistry>,
        egress: HashMap<String, Arc<ProxyClient>>,
    ) -> Result<Arc<Engine>, llm_proxy_engine::EngineError> {
        let config = llm_proxy_engine::ConfigSource::load(&*self.config_source)
            .unwrap_or_else(|_| Arc::clone(&self.app_config));
        let mut builder = EngineBuilder::from_parts(
            Arc::clone(&self.config_source) as Arc<dyn llm_proxy_engine::ConfigSource>,
            Arc::clone(providers),
            Arc::clone(&self.provider_adapters),
            Arc::clone(&self.proxy_client),
        )
        .metrics(Arc::clone(&self.metrics))
        .catalogs(Arc::clone(&self.model_catalogs))
        .event_bus(Arc::clone(&self.event_bus))
        .counter(self.token_counter.clone());
        // The one admission step (plan 05): per-key policies from
        // [auth.clients] and caps from [provider.concurrency] — read off the
        // registry this engine routes through. With neither configured the
        // engine keeps its allow-all default. A replica-shared override
        // (plan 09 step 1) replaces it wholesale: same rules, shared
        // counters.
        if let Some(admission) = self.admission_override.clone() {
            builder = builder.admission(admission);
        } else {
            let admission = llm_proxy_engine::PolicyAdmission::from_config(&config.auth, providers);
            if !admission.is_noop() {
                builder = builder.admission(Arc::new(admission));
            }
        }
        if let Some(cooldowns) = self.cooldowns_override.clone() {
            builder = builder.cooldowns(cooldowns);
        }
        // The opt-in response cache (plan 06): off by default, so with
        // no [server.cache] the engine keeps its store-nothing default.
        if config.server.cache.enabled {
            builder = builder.cache(Arc::new(llm_proxy_engine::MemoryCache::new(
                config.server.cache.ttl,
                config.server.cache.max_entries,
            )));
        }
        if !egress.is_empty() {
            builder = builder.egress_clients(egress);
        }
        builder.build().map(Arc::new)
    }

    /// Swaps in a freshly built engine (plan 07 step 11): new requests use
    /// it; in-flight requests keep running on the engine they already hold.
    /// Returns the previous engine, if one had been built.
    #[must_use]
    pub fn swap_engine(&self, engine: Arc<Engine>) -> Option<Arc<Engine>> {
        self.engine.replace(engine).and_then(|result| result.ok())
    }

    /// Access the provider model catalog service.
    pub fn model_catalogs(&self) -> &ModelCatalogService {
        &self.model_catalogs
    }

    /// The runtime metrics registry (also exported by the admin surface's
    /// `/metrics` route and the opt-in OTLP push, plan 07 step 7).
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
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
                cache: Default::default(),
                idempotency: Default::default(),
                metrics: Default::default(),
                log_format: Default::default(),
                admin_bind: None,
                admin_on_main_listener: false,
                endpoint_families: Default::default(),
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
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
            protocol_names
                .iter()
                .any(|name| name == "openai_chat_completions"),
            "missing openai_chat_completions adapter"
        );
        assert!(
            protocol_names
                .iter()
                .any(|name| name == "anthropic_messages"),
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: "https://example.com/v1".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                        forward_inbound_headers: None,
                        lossy: false,
                    },
                );
                m.insert(
                    "messages".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "anthropic_messages".to_owned(),
                        endpoint: "https://example.com/v1/messages".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                        forward_inbound_headers: None,
                        lossy: false,
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "fake".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "not_a_real_protocol".to_owned(),
                        endpoint: "https://example.com".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                        forward_inbound_headers: None,
                        lossy: false,
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "chat".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "openai_chat_completions".to_owned(),
                        endpoint: "https://example.com/v1".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                        forward_inbound_headers: None,
                        lossy: false,
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
        };

        let bad = ProviderConfig {
            name: "bad".to_owned(),
            api_key: secrecy::SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: {
                let mut m = HashMap::new();
                m.insert(
                    "fake".to_owned(),
                    ProviderAdapterConfig {
                        protocol: "not_real".to_owned(),
                        endpoint: "https://example.com".to_owned(),
                        headers: std::sync::Arc::new(HashMap::new()),
                        forward_inbound_headers: None,
                        lossy: false,
                    },
                );
                m
            },
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
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
            llm_proxy_core::ClientKeySpec::Verifier(secrecy::SecretString::from(
                "not-a-phc-string",
            )),
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
        cfg.auth.clients.insert(
            "laptop".to_owned(),
            llm_proxy_core::ClientKeySpec::Verifier(secrecy::SecretString::from(phc)),
        );
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
