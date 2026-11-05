//! The engine's extension points (audit conflict 13): every feature that
//! needs process-shared or backend-specific state implements one of these
//! traits instead of living in the axum layer. Each trait ships a local,
//! in-process default so a single node runs with no external services; the
//! Redis/Postgres-backed implementations arrive as optional crates and slot
//! in behind the same trait.
//!
//! [`EventBus`](llm_proxy_storage::EventBus) (usage events) and
//! [`Tokenizer`](llm_proxy_core::Counter) (token estimation) already existed
//! and are not duplicated here.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use llm_proxy_core::{AppConfig, ProviderRegistry, ProviderRouteKind};
use llm_proxy_protocol::core::{CoreEvent, CoreRequest, CoreResponse};
use secrecy::SecretString;

use crate::catalog::ModelCatalogService;
use crate::error::EngineError;

/// Resolves a route to a provider target (pipeline slot 3). The default
/// implementation is the TOML/config-loaded [`ProviderRegistry`] plus catalog
/// enforcement.
#[async_trait]
pub trait Router: Debug + Send + Sync {
    /// Resolve `provider` + route kind + requested model to the target the
    /// attempt loop encodes against.
    async fn resolve(&self, query: &RouteQuery<'_>) -> Result<ResolvedTarget, EngineError>;
}

/// Everything the router is asked per request.
#[derive(Debug, Clone, Copy)]
pub struct RouteQuery<'a> {
    /// The provider named in the URL (or pool, once pools exist).
    pub provider: &'a str,
    /// The endpoint family the client called.
    pub route_kind: ProviderRouteKind,
    /// The model name the client asked for.
    pub requested_model: &'a str,
}

/// The routing decision: which adapter config serves this request.
#[derive(Clone)]
pub struct ResolvedTarget {
    /// The provider config's resolved adapter target (endpoint, protocol,
    /// auth style, static headers, model aliasing, `lossy`, and the inbound
    /// header forwarding allowlist).
    pub target: llm_proxy_core::ProviderAdapterTargetConfig,
}

impl Debug for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The target carries the upstream credential; never include it.
        f.debug_struct("ResolvedTarget")
            .field("provider_name", &self.target.provider_name)
            .field("protocol", &self.target.protocol)
            .field("adapter_name", &self.target.adapter_name)
            .field("upstream_model", &self.target.upstream_model)
            .finish()
    }
}

/// The default [`Router`]: the loaded [`ProviderRegistry`], with catalog
/// enforcement for providers that opt in.
#[derive(Debug)]
pub struct RegistryRouter {
    registry: Arc<ProviderRegistry>,
    catalogs: Arc<ModelCatalogService>,
}

impl RegistryRouter {
    /// Wrap a registry and the catalog service.
    #[must_use]
    pub fn new(registry: Arc<ProviderRegistry>, catalogs: Arc<ModelCatalogService>) -> Self {
        Self { registry, catalogs }
    }
}

#[async_trait]
impl Router for RegistryRouter {
    async fn resolve(&self, query: &RouteQuery<'_>) -> Result<ResolvedTarget, EngineError> {
        let target = self
            .registry
            .resolve_provider_route(query.provider, query.route_kind, query.requested_model)
            .map_err(EngineError::from_route_error)?;
        if let Some(provider) = self.registry.get(query.provider)
            && provider
                .catalog
                .as_ref()
                .is_some_and(|catalog| catalog.enforce)
        {
            let allowed = self
                .catalogs
                .contains_model(provider, &target.upstream_model, &target.protocol)
                .await
                .map_err(|error| EngineError::Internal(error.to_string()))?;
            if !allowed {
                return Err(EngineError::ModelNotAllowed(target.upstream_model.clone()));
            }
        }
        Ok(ResolvedTarget { target })
    }
}

/// One admission step: reserve budget up front, then settle or refund
/// (audit conflict 2's resolution). The default admits everything.
///
/// `reserve` is async (it may consult a backend budget store); `settle` is
/// synchronous fire-and-forget accounting, mirroring the contract of
/// [`EventBus::emit`](llm_proxy_storage::EventBus::emit): implementations
/// must not block the calling task.
#[async_trait]
pub trait Admission: Debug + Send + Sync {
    /// Reserve capacity (budget, tokens, concurrency) before egress. Returns
    /// an opaque ticket handed back at [`Admission::settle`].
    async fn reserve(&self, claim: &AdmissionClaim<'_>) -> Result<AdmissionTicket, EngineError> {
        let _ = claim;
        Ok(AdmissionTicket::default())
    }

    /// Settle or refund the reservation once the attempt outcome is known.
    /// Called exactly once per successful [`Admission::reserve`].
    fn settle(&self, ticket: &AdmissionTicket, outcome: &AdmissionOutcome<'_>) {
        let _ = (ticket, outcome);
    }
}

/// What is being admitted.
#[derive(Debug)]
pub struct AdmissionClaim<'a> {
    /// The caller identity (key id or `"anonymous"`).
    pub caller: &'a str,
    /// Provider the request routes to.
    pub provider: &'a str,
    /// Requested model.
    pub model: &'a str,
    /// SHA-256 of the raw client body (never the body itself).
    pub body_hash: &'a str,
}

/// An admission reservation; implementations carry what they need to settle.
#[derive(Debug, Default, Clone)]
pub struct AdmissionTicket {
    /// Reserved units, if the implementation accounts in tokens/credits.
    pub reserved: Option<u64>,
}

/// The outcome handed back at settlement.
#[derive(Debug)]
pub struct AdmissionOutcome<'a> {
    /// Whether the request completed.
    pub succeeded: bool,
    /// Final usage, when the attempt produced one.
    pub usage: Option<&'a llm_proxy_protocol::core::Usage>,
}

/// The default [`Admission`]: everything is admitted, nothing is accounted.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllAdmission;

#[async_trait]
impl Admission for AllowAllAdmission {}

/// Response cache lookup (slot 5) and store (slot 13). The default caches
/// nothing. Keys are tenant-safe by construction: caller, provider, route,
/// model and body digest.
#[async_trait]
pub trait Cache: Debug + Send + Sync {
    /// Look up a stored response for this key.
    async fn lookup(&self, key: &CacheKey<'_>) -> Result<Option<CacheEntry>, EngineError> {
        let _ = key;
        Ok(None)
    }

    /// Store a completed response under this key.
    async fn store(&self, key: &CacheKey<'_>, entry: &CacheEntry) {
        let _ = (key, entry);
    }
}

/// A cache key: identifies one normalized request, caller-scoped.
#[derive(Debug)]
pub struct CacheKey<'a> {
    /// The caller identity (key id or `"anonymous"`).
    pub caller: &'a str,
    /// Provider the request routed to.
    pub provider: &'a str,
    /// The endpoint family.
    pub route_kind: ProviderRouteKind,
    /// The model name sent upstream (after aliasing).
    pub model: &'a str,
    /// SHA-256 of the raw client body.
    pub body_hash: &'a str,
    /// Whether the client asked for a stream.
    pub streaming: bool,
}

/// A cached response.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The stored core response (a stream is replayed from its collected
    /// events).
    pub response: CoreResponse,
    /// The collected core events of a streamed response, in order.
    pub events: Vec<CoreEvent>,
}

/// The default [`Cache`]: nothing is stored or answered.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCache;

#[async_trait]
impl Cache for NoCache {}

/// A guardrail hook. Modes follow audit conflict 9: pre-call (before egress),
/// post-call (non-streaming response), and per-event (streaming), with the
/// verdict deciding rejection; buffering/redaction modes are a later plan.
#[async_trait]
pub trait Guard: Debug + Send + Sync {
    /// Inspect the normalized request before egress.
    async fn inspect_request(&self, request: &CoreRequest) -> Result<GuardDecision, EngineError> {
        let _ = request;
        Ok(GuardDecision::Allow)
    }

    /// Inspect a completed non-streaming response.
    async fn inspect_response(
        &self,
        response: &CoreResponse,
    ) -> Result<GuardDecision, EngineError> {
        let _ = response;
        Ok(GuardDecision::Allow)
    }

    /// Inspect each streamed core event as it is decoded.
    async fn inspect_event(&self, event: &CoreEvent) -> Result<GuardDecision, EngineError> {
        let _ = event;
        Ok(GuardDecision::Allow)
    }
}

/// What a guard decides.
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// Let the request/response/event through.
    Allow,
    /// Reject: the message is surfaced to the client as the rejection reason.
    Reject(String),
}

/// Supplies the upstream credential for an attempt (slot 9's credential
/// pick). The default is the static key from provider config, with
/// client-token passthrough when the provider opts in.
#[async_trait]
pub trait CredentialSource: Debug + Send + Sync {
    /// Pick the credential for `provider`.
    ///
    /// `configured` is the static key from provider config; `inbound` is the
    /// client's own token, present only when the caller sent one. A rotation
    /// implementation ignores both or uses them as seeds.
    async fn credential(
        &self,
        provider: &str,
        configured: &SecretString,
        passthrough: bool,
        inbound: Option<&SecretString>,
    ) -> Result<SecretString, EngineError>;
}

/// The default [`CredentialSource`]: the configured static key, or the
/// client's token on `passthrough_auth` providers (which requires one).
#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigCredentialSource;

#[async_trait]
impl CredentialSource for ConfigCredentialSource {
    async fn credential(
        &self,
        provider: &str,
        configured: &SecretString,
        passthrough: bool,
        inbound: Option<&SecretString>,
    ) -> Result<SecretString, EngineError> {
        let _ = provider;
        if passthrough {
            inbound.cloned().ok_or(EngineError::Unauthorized)
        } else {
            Ok(configured.clone())
        }
    }
}

/// Where usage rows go (slot 13). The default forwards to the
/// [`EventBus`](llm_proxy_storage::EventBus) the app configured (SQLite
/// ledger or [`NoopBus`](llm_proxy_storage::NoopBus)).
pub trait UsageSink: Debug + Send + Sync {
    /// Record one lifecycle event.
    fn record(&self, event: &llm_proxy_storage::ProxyEvent);
}

/// The default [`UsageSink`]: delegate to an event bus.
#[derive(Debug)]
pub struct EventBusSink {
    bus: Arc<dyn llm_proxy_storage::EventBus>,
}

impl EventBusSink {
    /// Wrap an event bus.
    #[must_use]
    pub fn new(bus: Arc<dyn llm_proxy_storage::EventBus>) -> Self {
        Self { bus }
    }

    /// The wrapped bus.
    #[must_use]
    pub fn bus(&self) -> &Arc<dyn llm_proxy_storage::EventBus> {
        &self.bus
    }
}

impl UsageSink for EventBusSink {
    fn record(&self, event: &llm_proxy_storage::ProxyEvent) {
        self.bus.emit(event);
    }
}

/// Supplies the configuration snapshot (audit conflict 11). TOML files are
/// one source; programmatic config is another, and a record-store backend
/// (key API, hot reload) is a later plan implementing this same trait.
pub trait ConfigSource: Debug + Send + Sync {
    /// The current configuration snapshot.
    fn load(&self) -> Result<Arc<AppConfig>, EngineError>;
}

/// A [`ConfigSource`] over one immutable [`AppConfig`] snapshot — what a
/// TOML file or a builder produces.
#[derive(Debug, Clone)]
pub struct StaticConfigSource {
    config: Arc<AppConfig>,
}

impl StaticConfigSource {
    /// Wrap a config snapshot.
    #[must_use]
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

impl ConfigSource for StaticConfigSource {
    fn load(&self) -> Result<Arc<AppConfig>, EngineError> {
        Ok(Arc::clone(&self.config))
    }
}

/// Inbound headers as the engine sees them: names lowercased, UTF-8 values
/// only. The server performs the conversion from its HTTP layer.
pub type InboundHeaders = HashMap<String, String>;
