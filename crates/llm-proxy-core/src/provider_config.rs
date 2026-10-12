//! TOML provider configuration types, parsing, env-var interpolation, and
//! validation for the routing system.
//!
//! This is the sole configuration system for the proxy, loaded from the main
//! `config.toml` file. The main file contains server settings; provider files
//! contain provider-scoped routing and catalog configuration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::env_interpolate::{find_env_var_refs, find_unresolved_env_var, interpolate_env_vars};
use crate::error::CoreError;

// ---------------------------------------------------------------------------
// Main config: AppConfig
// ---------------------------------------------------------------------------

/// Top-level TOML application configuration.
///
/// Loaded from the main `config.toml`, which contains server and auth
/// settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Server bind and operational settings.
    pub server: ServerConfig,
    /// Client authentication settings (`[auth]` section).
    #[serde(default)]
    pub auth: AuthConfig,
}

// ---------------------------------------------------------------------------
// ServerConfig
// ---------------------------------------------------------------------------

/// Log output format for the tracing subscriber.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable plain-text logs (default).
    #[default]
    Plain,
    /// Structured JSON logs (one JSON object per line).
    Json,
}

impl LogFormat {
    /// Read the format from the `RUST_LOG_FORMAT` env var (case-insensitive).
    ///
    /// Defaults to [`LogFormat::Plain`] when unset or unrecognized. Read at
    /// subscriber init (before config load), consistent with `RUST_LOG`.
    pub fn from_env() -> Self {
        match std::env::var("RUST_LOG_FORMAT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "json" => Self::Json,
            _ => Self::Plain,
        }
    }
}

/// Server operational settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Maximum request duration before timeout.
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Hard deadline for graceful shutdown.
    ///
    /// After receiving SIGINT/SIGTERM the server stops accepting new
    /// connections and waits this long for in-flight requests to drain.
    /// Because the proxy serves long-lived streaming responses that an
    /// upstream can hold open arbitrarily, an unbounded drain would let a
    /// single stuck stream pin the process indefinitely. If the deadline
    /// elapses the process force-exits so orchestrators (Kubernetes/LB)
    /// can recycle the pod. `0s` disables the hard deadline (drain
    /// forever) but is strongly discouraged.
    #[serde(default = "default_shutdown_timeout", with = "humantime_serde")]
    pub shutdown_timeout: Duration,
    /// Log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
    pub log_level: String,
    /// Log output format for the tracing subscriber: `"plain"` (default) or
    /// `"json"`. Applied at subscriber init from the `RUST_LOG_FORMAT` env var
    /// (see [`LogFormat::from_env`]); this field documents the intended default.
    #[serde(default)]
    pub log_format: LogFormat,
    /// Reserved hot-reload flag. Configuration loading rejects `true`.
    pub hot_reload: bool,
    /// Per-client requests allowed per minute. `0` disables rate limiting.
    #[serde(default = "default_rate_limit_rpm")]
    pub rate_limit_rpm: u32,
    /// Whether client identity may use `X-Forwarded-For` and `X-Real-IP`.
    ///
    /// # Trust model
    ///
    /// When true, the **leftmost** value of `X-Forwarded-For` (and the value of
    /// `X-Real-IP`) is trusted as the client address. This is only correct
    /// behind a **single** reverse proxy that strips or overwrites any inbound
    /// `X-Forwarded-For` before appending its own. With multiple untrusted hops
    /// the leftmost entry is attacker-controllable (a client can prepend
    /// arbitrary values), so the extracted address must not be trusted for
    /// authentication, rate-limit bypass, or audit logging.
    #[serde(default)]
    pub trust_forwarded_headers: bool,
    /// Window for rejecting identical path/body requests. `0s` disables deduplication.
    #[serde(default = "default_dedup_window")]
    #[serde(with = "humantime_serde")]
    pub dedup_window: Duration,
    /// Public server name reported in version/health endpoints.
    pub server_name: String,
    /// Cross-origin origins permitted to call the proxy from a browser
    /// (audit LOW-12).
    ///
    /// `None` (the default) installs **no** CORS layer, so browsers enforce a
    /// same-origin policy and this server-to-server proxy is not unexpectedly
    /// reachable from arbitrary web origins. Setting a list installs a
    /// restrictive `CorsLayer` that echoes `Access-Control-Allow-Origin` only
    /// for these exact origins -- replacing a former `very_permissive` policy
    /// that admitted every origin.
    ///
    /// Each entry should be an absolute origin (`scheme://host[:port]`),
    /// e.g. `"https://app.example.com"`. Entries that fail to parse as a
    /// header value are skipped (with a warning) when the router is built.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
}

const fn default_rate_limit_rpm() -> u32 {
    100
}

// ---------------------------------------------------------------------------
// AuthConfig
// ---------------------------------------------------------------------------

/// Client authentication mode for proxy routes.
///
/// Serialized in TOML as `"none"` / `"client-keys"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// No client authentication: every request is attributed to the
    /// `"anonymous"` caller. Only permitted while the server binds loopback;
    /// `load_app_config` rejects a non-loopback bind in this mode.
    #[default]
    None,
    /// Requests to proxy routes must present
    /// `Authorization: Bearer <client-key>` matching a secret hashed under
    /// `[auth.clients]`.
    ClientKeys,
}

/// Client authentication configuration (`[auth]` section of `config.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Authentication mode. Defaults to [`AuthMode::None`].
    #[serde(default)]
    pub mode: AuthMode,
    /// Key id to hashed client secret, under `[auth.clients]`.
    ///
    /// Each value is a PHC-format password-hashing string (argon2id), never a
    /// plaintext secret. Wrapped in [`SecretString`] so the value is redacted
    /// in `Debug` output.
    ///
    /// `skip_serializing` (mirroring `ProviderConfig::api_key`) prevents the
    /// hashes from leaking through any `Serialize` path; deserializing a
    /// config that was serialized will therefore fail on this field, which is
    /// intentional.
    #[serde(default, skip_serializing)]
    pub clients: HashMap<String, SecretString>,
}

/// Default graceful-shutdown hard deadline (30 seconds).
///
/// Chosen to comfortably exceed the maximum upstream request timeout while
/// bounding how long a stuck streaming connection can delay process recycle.
const fn default_shutdown_timeout() -> Duration {
    Duration::from_secs(30)
}

const fn default_dedup_window() -> Duration {
    Duration::ZERO
}

// ---------------------------------------------------------------------------
// ProviderFile
// ---------------------------------------------------------------------------

/// Wrapper for a single provider TOML file.
///
/// Each provider is defined in its own file with a top-level `[provider]`
/// section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFile {
    /// The provider definition.
    pub provider: ProviderConfig,
}

// ---------------------------------------------------------------------------
// ProviderConfig (custom Debug to redact api_key)
// ---------------------------------------------------------------------------

/// Provider configuration loaded from a dedicated TOML file.
///
/// The `api_key` field is redacted in [`Debug`] output and excluded from
/// [`Serialize`] output to prevent credential leakage in logs, error messages,
/// and API responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Provider name (e.g. `"opencode-go"`, `"opencode-zen"`).
    pub name: String,
    /// API key for authenticating with the upstream provider.
    /// Supports `${ENV_VAR}` interpolation (applied to the raw TOML *before*
    /// this field is constructed, so the resolved value is what gets wrapped).
    ///
    /// Wrapped in [`SecretString`]: redacted in [`Debug`] and [`Display`] output
    /// and zeroized on drop. Not serialized (`skip_serializing`) -- use explicit
    /// methods if you need to write a config file.
    ///
    /// WHY `skip_serializing`: prevents credentials from leaking through
    /// serialization paths (e.g. debug logs, API responses, config dumps).
    /// Round-tripping a `ProviderFile` through serialize/deserialize will
    /// FAIL because `skip_serializing` omits `api_key` and `api_key` is a
    /// required field. This is intentional -- credentials should never
    /// round-trip through serialization.
    #[serde(skip_serializing)]
    pub api_key: SecretString,
    /// Authentication header style.
    pub auth_style: AuthStyle,
    /// When true, ignore this provider's configured `api_key` and instead use
    /// the inbound client's auth token (the `Authorization: Bearer ...` or
    /// `x-api-key` request header) as the upstream credential
    /// (bring-your-own-key / passthrough auth). The request fails with 401 if
    /// the client sends no recognizable token. When false (default), the
    /// provider's static `api_key` is used.
    #[serde(default)]
    pub passthrough_auth: bool,
    /// Named adapter configurations (protocol + endpoint pairs).
    #[serde(default)]
    pub adapters: HashMap<String, ProviderAdapterConfig>,
    /// Route kind to adapter name mapping (e.g. chat_completions -> openai_adapter).
    #[serde(default)]
    pub routes: ProviderRoutesConfig,
    /// Model alias mapping (requested model -> upstream model name).
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    /// Live model discovery configuration (optional).
    #[serde(default)]
    pub discovery: Option<ProviderDiscoveryConfig>,
    /// Model catalog configuration (optional, defaults to advisory hybrid).
    #[serde(default)]
    pub catalog: Option<ProviderCatalogConfig>,
    /// Per-model pricing, keyed by upstream model id. Used for cost estimation
    /// only; not enforced for routing. Aliases are resolved before lookup, so
    /// the key is the upstream model id. Defaults to empty (no cost accounting).
    #[serde(default)]
    pub pricing: HashMap<ModelId, ModelPricing>,
}

// ---------------------------------------------------------------------------
// ModelId / ModelPricing
// ---------------------------------------------------------------------------

/// Provider-local model identifier used as a pricing key.
///
/// Transparent newtype over `String` so it serializes as a plain string in
/// TOML/JSON. Intentionally minimal: existing `String` model fields elsewhere
/// are not migrated to this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(
    /// The model id string (the upstream model id, after alias resolution).
    pub String,
);

impl ModelId {
    /// Create a model id from anything string-like.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    /// The model id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-token price for one model, in USD.
///
/// All fields are USD per token as [`Decimal`] to avoid floating-point money. A
/// price of `0` means "unmetered". `cache_creation`, `cache_read`, and
/// `reasoning` default to `0` and may be omitted.
///
/// **Operators must always quote decimal values in TOML** (e.g.
/// `input = "0.0000014"`). An unquoted `0.0000014` is parsed by TOML as an
/// `f64` (losing precision), and `rust_decimal::Decimal` rejects floats, so
/// deserialization fails with a confusing error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    /// USD per input token.
    pub input: Decimal,
    /// USD per output token.
    pub output: Decimal,
    /// USD per cache-creation input token.
    #[serde(default)]
    pub cache_creation: Decimal,
    /// USD per cache-read input token.
    #[serde(default)]
    pub cache_read: Decimal,
    /// USD per reasoning token.
    #[serde(default)]
    pub reasoning: Decimal,
}

/// Upper bound for any per-token USD price accepted by config validation.
///
/// Every real per-token price is far below $1; values above $1000/token are
/// treated as gross misconfigurations (stray exponents, per-1M-token prices
/// entered verbatim, misplaced decimals) and rejected at load time. See
/// [`validate_provider_config`]. `compute_cost` uses checked arithmetic on top
/// of this, so the proxy can never panic on overflow regardless.
///
/// Returned by a function rather than a `const` because `Decimal` has no
/// const constructor at this rust_decimal version (`Decimal::new` is not const).
fn max_per_token_price() -> Decimal {
    Decimal::new(1_000, 0)
}

// ---------------------------------------------------------------------------
// AuthStyle
// ---------------------------------------------------------------------------

/// How the API key is sent in HTTP headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
    /// `x-goog-api-key: <key>` (Google/Gemini providers).
    XGoogleApiKey,
    /// Both `Authorization: Bearer <key>` and `x-api-key: <key>`.
    Both,
}

// ---------------------------------------------------------------------------
// ProviderAdapterConfig
// ---------------------------------------------------------------------------

/// Serialize an `Arc<HashMap<String, String>>` by value (LOW-5).
///
/// serde only implements `Serialize`/`Deserialize` for `Arc<T>` behind its `rc`
/// feature, which this crate deliberately leaves OFF (it is a footgun: it
/// deserializes `Rc`/`Arc` by reference-sharing). These helpers give the shared
/// headers map ordinary by-value (de)serialization without enabling that feature.
fn serialize_arc_headers<S>(
    headers: &Arc<HashMap<String, String>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    headers.as_ref().serialize(serializer)
}

/// Deserialize a headers map into a fresh `Arc` (by value, not reference-shared).
fn deserialize_arc_headers<'de, D>(
    deserializer: D,
) -> Result<Arc<HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    HashMap::<String, String>::deserialize(deserializer).map(Arc::new)
}

/// A single adapter entry within a provider.
///
/// Maps a protocol name to an upstream endpoint URL.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAdapterConfig {
    /// Protocol family (e.g. `"openai_chat_completions"`, `"anthropic_messages"`).
    pub protocol: String,
    /// Upstream endpoint URL.
    ///
    /// **Warning:** This field may contain sensitive query parameters (e.g. API
    /// keys passed as URL query strings). Avoid logging or displaying the raw
    /// value. The [`Debug`](std::fmt::Debug) impl strips query parameters for
    /// safe display.
    pub endpoint: String,
    /// Static headers to include in upstream requests (e.g. anthropic-version).
    ///
    /// Wrapped in [`Arc`] because adapter headers are fixed at config-load time
    /// and shared across every request that resolves to this adapter; resolving
    /// a route then bumps the refcount instead of deep-cloning the map (LOW-5).
    /// Serde deserializes this by value (wrapping the parsed `HashMap` in
    /// `Arc::new`) without the `rc` feature.
    #[serde(
        default,
        serialize_with = "serialize_arc_headers",
        deserialize_with = "deserialize_arc_headers"
    )]
    pub headers: Arc<HashMap<String, String>>,
}

impl std::fmt::Debug for ProviderAdapterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAdapterConfig")
            .field("protocol", &self.protocol)
            .field("endpoint", &endpoint_without_query(&self.endpoint))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub(crate) fn endpoint_without_query(endpoint: &str) -> &str {
    endpoint
        .split_once('?')
        .map_or(endpoint, |(base, _query)| base)
}

// ---------------------------------------------------------------------------
// ProviderRouteKind & ProviderRoutesConfig
// ---------------------------------------------------------------------------

/// The kind of client-facing route being requested.
///
/// Each variant corresponds to an inbound endpoint that the proxy exposes.
/// Providers map these route kinds to adapter names via [`ProviderRoutesConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRouteKind {
    /// OpenAI-compatible `/chat/completions` endpoint.
    ChatCompletions,
    /// Anthropic-compatible `/messages` endpoint.
    Messages,
}

/// Maps [`ProviderRouteKind`] to adapter names within a provider.
///
/// Defined in TOML as:
///
/// ```toml
/// [provider.routes]
/// chat_completions = "openai_adapter"
/// messages = "anthropic_adapter"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRoutesConfig {
    /// Route kind: adapter name for the `chat_completions` endpoint.
    pub chat_completions: Option<String>,
    /// Route kind: adapter name for the `messages` endpoint.
    pub messages: Option<String>,
}

impl ProviderRoutesConfig {
    /// Look up the adapter name for the given route kind.
    pub fn get(&self, kind: ProviderRouteKind) -> Option<&str> {
        match kind {
            ProviderRouteKind::ChatCompletions => self.chat_completions.as_deref(),
            ProviderRouteKind::Messages => self.messages.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// ProviderDiscoveryKind & ProviderDiscoveryConfig
// ---------------------------------------------------------------------------

/// Supported discovery backends for fetching upstream model lists.
///
/// Provider-specific discovery kinds are only justified when authentication,
/// pagination, or response shape differs materially from the OpenAI-compatible
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDiscoveryKind {
    /// Generic OpenAI-style `GET /v1/models` returning `{ "data": [{ "id": ... }] }`.
    #[serde(rename = "openai_compatible_models")]
    OpenAiCompatibleModels,
    /// OpenAI's own model-list endpoint (uses org-level auth).
    #[serde(rename = "openai_models")]
    OpenAiModels,
    /// Anthropic model-list endpoint (`GET /v1/models` with `x-api-key`).
    AnthropicModels,
    /// Google Gemini model-list endpoint.
    GeminiModels,
    /// Fireworks account-level model-list endpoint.
    FireworksAccountModels,
}

/// Configuration for live model discovery from an upstream provider.
///
/// Discovery does **not** run automatically during server startup. Live refresh
/// is triggered explicitly via CLI or a `refresh=live` query parameter on the
/// models endpoint.
///
/// ```toml
/// [provider.discovery]
/// kind = "openai_compatible_models"
/// endpoint = "https://api.example.com/v1/models"
/// max_pages = 100
/// max_models = 20000
/// max_response_bytes = 4194304
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDiscoveryConfig {
    /// Which discovery backend protocol to use.
    pub kind: ProviderDiscoveryKind,
    /// Upstream URL to fetch the model list from.
    ///
    /// **Warning:** This field may contain sensitive query parameters. Avoid
    /// logging or displaying the raw value. The [`Debug`](std::fmt::Debug) impl
    /// strips query parameters for safe display.
    pub endpoint: String,
    /// Static headers used only for discovery requests.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Maximum number of pages fetched during one discovery refresh.
    #[serde(default = "default_discovery_max_pages")]
    pub max_pages: usize,
    /// Maximum number of model records accepted during one discovery refresh.
    #[serde(default = "default_discovery_max_models")]
    pub max_models: usize,
    /// Maximum response body size accepted for each discovery page.
    #[serde(default = "default_discovery_max_response_bytes")]
    pub max_response_bytes: usize,
}

const fn default_discovery_max_pages() -> usize {
    100
}

const fn default_discovery_max_models() -> usize {
    20_000
}

const fn default_discovery_max_response_bytes() -> usize {
    4 * 1024 * 1024
}

impl std::fmt::Debug for ProviderDiscoveryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDiscoveryConfig")
            .field("kind", &self.kind)
            .field("endpoint", &endpoint_without_query(&self.endpoint))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("max_pages", &self.max_pages)
            .field("max_models", &self.max_models)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ProviderCatalogMode & StaticModelCatalogEntry & ProviderCatalogConfig
// ---------------------------------------------------------------------------

/// How the model catalog is assembled for a provider.
///
/// - `Static`: only use operator-defined `[[provider.catalog.models]]` entries.
/// - `Discovered`: only use live/cached discovery results.
/// - `Hybrid`: merge both sources; static metadata wins on ID conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCatalogMode {
    /// Only use `[[provider.catalog.models]]` entries.
    Static,
    /// Only use live/cached discovery results.
    Discovered,
    /// Merge static and discovered models; static metadata wins on ID conflicts.
    #[serde(rename = "hybrid")]
    #[default]
    Hybrid,
}

/// A single static model entry in the provider catalog.
///
/// Defined in TOML as:
///
/// ```toml
/// [[provider.catalog.models]]
/// id = "accounts/fireworks/models/deepseek-v3p1"
/// display_name = "DeepSeek V3.1"
/// supports = ["chat_completions"]
/// context_length = 131_072
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelCatalogEntry {
    /// Model ID (must be non-empty, matches the upstream model name).
    pub id: String,
    /// Human-readable display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Which route kinds this model supports.
    #[serde(default)]
    pub supports: Vec<ProviderRouteKind>,
    /// Maximum context length in tokens, if known.
    #[serde(default)]
    pub context_length: Option<u32>,
}

/// Provider catalog configuration.
///
/// Controls how the `/providers/{provider}/v1/models` endpoint assembles its
/// response and whether catalog enforcement blocks unknown model IDs.
///
/// ```toml
/// [provider.catalog]
/// mode = "hybrid"
/// enforce = false
/// cache_ttl = "24h"
/// allow = ["*"]
/// deny = ["*-preview"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCatalogConfig {
    /// How to assemble the catalog: static, discovered, or hybrid merge.
    #[serde(default)]
    pub mode: ProviderCatalogMode,
    /// When `true`, requests using a model ID not in the filtered catalog are
    /// rejected. When `false` (default), the catalog is advisory only.
    #[serde(default)]
    pub enforce: bool,
    /// How long to cache discovered models before allowing a refresh.
    #[serde(default = "default_cache_ttl")]
    #[serde(with = "humantime_serde")]
    pub cache_ttl: Duration,
    /// Glob patterns for model IDs that are allowed. `["*"]` allows all.
    #[serde(default = "default_allow_all")]
    pub allow: Vec<String>,
    /// Glob patterns for model IDs that are denied unless a non-wildcard allow
    /// pattern explicitly includes the model.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Operator-defined static model entries.
    #[serde(default)]
    pub models: Vec<StaticModelCatalogEntry>,
}

fn default_cache_ttl() -> Duration {
    Duration::from_secs(24 * 60 * 60) // 24 hours
}

fn default_allow_all() -> Vec<String> {
    vec!["*".to_owned()]
}

impl Default for ProviderCatalogConfig {
    fn default() -> Self {
        Self {
            mode: ProviderCatalogMode::Hybrid,
            enforce: false,
            cache_ttl: default_cache_ttl(),
            allow: default_allow_all(),
            deny: Vec::new(),
            models: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Validation error
// ---------------------------------------------------------------------------

/// Errors produced during config validation.
///
/// This enum is `#[non_exhaustive]` so validation can evolve without breaking
/// downstream `match` expressions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigValidationError {
    /// An adapter `endpoint` URL is empty.
    #[error("provider \"{provider}\": adapter \"{adapter}\" has an empty endpoint")]
    EmptyEndpoint {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
    },
    /// An environment variable referenced in `api_key` could not be resolved.
    #[error(
        "provider \"{provider}\": api_key contains unresolvable environment variable \"{var}\""
    )]
    UnresolvedEnvVar {
        /// Provider name.
        provider: String,
        /// The `${VAR}` pattern that could not be resolved.
        var: String,
    },
    /// An environment variable resolved to an empty value.
    ///
    /// This is emitted by [`load_provider_config`], which scans the entire raw
    /// TOML for `${VAR}` references (any field: `endpoint`, `protocol`, `name`,
    /// `headers`, `api_key`, ...), not just `api_key`. The message is therefore
    /// field-agnostic; it does not name a specific field.
    #[error(
        "provider \"{provider}\": environment variable \"{var}\" referenced in the provider config resolved to an empty value"
    )]
    EmptyEnvVar {
        /// Provider name.
        provider: String,
        /// The environment variable name.
        var: String,
    },
    /// The literal `api_key` value (after interpolation) is empty.
    #[error("provider \"{provider}\": api_key is empty")]
    EmptyApiKey {
        /// Provider name.
        provider: String,
    },
    /// A pricing entry is invalid (empty model id or negative price).
    #[error("provider \"{provider}\": pricing for model \"{model}\" is invalid: {message}")]
    InvalidPricing {
        /// Provider name.
        provider: String,
        /// Model id whose pricing is invalid.
        model: String,
        /// What is wrong (e.g. "negative input price").
        message: String,
    },
    /// A provider name is empty.
    #[error("provider name is empty")]
    EmptyProviderName,
    /// A catalog model entry has an empty ID.
    #[error("provider \"{provider}\": catalog entry has an empty id")]
    EmptyCatalogEntryId {
        /// Provider name.
        provider: String,
    },
    /// An adapter name is empty.
    #[error("provider \"{provider}\": adapter name is empty")]
    EmptyAdapterName {
        /// Provider name.
        provider: String,
    },
    /// An adapter has an empty protocol.
    #[error("provider \"{provider}\": adapter \"{adapter}\" has an empty protocol")]
    EmptyProtocol {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
    },
    /// A protocol is not in the known set supplied by the caller.
    #[error("provider \"{provider}\": adapter \"{adapter}\" uses unknown protocol \"{protocol}\"")]
    UnknownProtocol {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
        /// Protocol name.
        protocol: String,
    },
    /// An adapter endpoint has a non-HTTP(S) URL scheme.
    #[error(
        "provider \"{provider}\": adapter \"{adapter}\" has invalid endpoint scheme (expected http:// or https://): \"{endpoint}\""
    )]
    InvalidEndpointScheme {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
        /// The endpoint URL that has an invalid scheme.
        endpoint: String,
    },
    /// An adapter header name or value contains CR or LF characters.
    #[error("provider \"{provider}\": adapter \"{adapter}\" header contains CR/LF characters")]
    HeaderContainsCrlf {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
    },
    /// An adapter header uses a forbidden transport header name.
    #[error("provider \"{provider}\": adapter \"{adapter}\" sets forbidden header \"{header}\"")]
    ForbiddenHeader {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
        /// The forbidden header name.
        header: String,
    },
    /// An adapter header name is empty.
    #[error("provider \"{provider}\": adapter \"{adapter}\" has an empty header name")]
    EmptyHeaderName {
        /// Provider name.
        provider: String,
        /// Adapter name.
        adapter: String,
    },
    /// A provider name contains invalid characters (must be URL-safe slug).
    #[error(
        "provider name \"{provider}\" contains invalid characters; \
         only lowercase ASCII letters, digits, hyphens, and underscores are allowed"
    )]
    InvalidProviderNameFormat {
        /// The invalid provider name.
        provider: String,
    },
    /// A route adapter name is empty.
    #[error("provider \"{provider}\": route \"{route_kind}\" has an empty adapter name")]
    EmptyRouteAdapterName {
        /// Provider name.
        provider: String,
        /// Route kind (e.g. "chat_completions" or "messages").
        route_kind: String,
    },
    /// A route references an adapter that does not exist.
    #[error(
        "provider \"{provider}\": route \"{route_kind}\" references unknown adapter \"{adapter}\""
    )]
    UnknownRouteAdapter {
        /// Provider name.
        provider: String,
        /// Route kind.
        route_kind: String,
        /// Adapter name.
        adapter: String,
    },
    /// A model alias key is empty.
    #[error("provider \"{provider}\": model alias key is empty")]
    EmptyModelAliasKey {
        /// Provider name.
        provider: String,
    },
    /// A model alias target is empty.
    #[error("provider \"{provider}\": model alias \"{alias}\" has an empty target")]
    EmptyModelAliasTarget {
        /// Provider name.
        provider: String,
        /// The alias key.
        alias: String,
    },
    /// A configured model ID cannot be safely interpolated into a Gemini URL template.
    #[error(
        "provider \"{provider}\": {location} model ID \"{model}\" contains characters that are unsafe for Gemini URL templates"
    )]
    UnsafeGeminiModelId {
        /// Provider name.
        provider: String,
        /// Configuration source, such as a catalog entry or alias target.
        location: &'static str,
        /// Unsafe model ID.
        model: String,
    },
    /// A discovery resource limit is zero.
    #[error("provider \"{provider}\": discovery limit \"{limit}\" must be greater than zero")]
    InvalidDiscoveryLimit {
        /// Provider name.
        provider: String,
        /// Invalid limit name.
        limit: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Validate a single [`ProviderConfig`].
///
/// Checks:
/// - provider name is non-empty (whitespace-only names are rejected)
/// - `api_key` is non-empty (after interpolation) and no unresolved `${VAR}` patterns remain
/// - all adapter names, protocol names, and endpoints are non-empty (whitespace-only values are rejected)
/// - adapter static headers reject CRLF characters and forbidden transport headers
/// - provider name is a URL-safe slug (lowercase ASCII letters, digits, hyphens, underscores)
/// - route adapter references point to existing adapters
/// - model alias keys and targets are non-empty
/// - static catalog IDs and alias targets used by Gemini URL templates are path-safe
///
/// `known_protocols` is an optional list of protocol names that are compiled
/// into the running binary. When `None`, protocol-name validation is skipped
/// (the core crate does not own the adapter registry). The server layer passes
/// `Some(ProviderAdapterRegistry::builtin().protocol_names())` for full validation.
///
/// The caller should ensure env-var interpolation has already been performed on
/// the `api_key` field before calling this function. If the raw `${VAR}` pattern
/// remains in `api_key`, it will be caught as an unresolved env var.
///
/// # Errors
///
/// Returns a [`ConfigValidationError`] variant describing the first validation
/// failure encountered. Checks run in a defined order: provider name, unresolved
/// env vars in `api_key`, empty `api_key`, adapter names/protocols/endpoints/headers,
/// protocol-name membership, route adapter references,
/// and model alias validation.
///
/// # Examples
///
/// A minimal, well-formed provider validates cleanly:
///
/// ```
/// use std::collections::HashMap;
/// use std::sync::Arc;
/// use llm_proxy_core::provider_config::{
///     AuthStyle, ProviderAdapterConfig, ProviderConfig, ProviderRoutesConfig,
///     validate_provider_config,
/// };
/// use secrecy::SecretString;
///
/// let provider = ProviderConfig {
///     name: "example".to_owned(),
///     api_key: SecretString::from("secret"),
///     auth_style: AuthStyle::Bearer,
///     passthrough_auth: false,
///     adapters: HashMap::from([(
///         "chat".to_owned(),
///         ProviderAdapterConfig {
///             protocol: "openai_chat_completions".to_owned(),
///             endpoint: "https://example.com/v1/chat/completions".to_owned(),
///             headers: Arc::new(HashMap::new()),
///         },
///     )]),
///     routes: ProviderRoutesConfig::default(),
///     model_aliases: HashMap::new(),
///     discovery: None,
///     catalog: None,
///     pricing: Default::default(),
/// };
/// assert!(validate_provider_config(&provider, None).is_ok());
/// ```
///
/// An empty provider name is rejected with [`ConfigValidationError::EmptyProviderName`]:
///
/// ```
/// use std::collections::HashMap;
/// use llm_proxy_core::provider_config::{
///     AuthStyle, ConfigValidationError, ProviderConfig, ProviderRoutesConfig,
///     validate_provider_config,
/// };
/// use secrecy::SecretString;
///
/// let provider = ProviderConfig {
///     name: "   ".to_owned(), // whitespace-only is treated as empty
///     api_key: SecretString::from("secret"),
///     auth_style: AuthStyle::Bearer,
///     passthrough_auth: false,
///     adapters: HashMap::new(),
///     routes: ProviderRoutesConfig::default(),
///     model_aliases: HashMap::new(),
///     discovery: None,
///     catalog: None,
///     pricing: Default::default(),
/// };
/// assert_eq!(
///     validate_provider_config(&provider, None),
///     Err(ConfigValidationError::EmptyProviderName),
/// );
/// ```
pub fn validate_provider_config(
    provider: &ProviderConfig,
    known_protocols: Option<&[&str]>,
) -> Result<(), ConfigValidationError> {
    let name = &provider.name;

    validate_name(name)?;
    validate_api_key(provider, name)?;
    validate_pricing(provider, name)?;
    validate_adapters(provider, name, known_protocols)?;
    validate_discovery(provider, name)?;

    // Both the catalog and the model aliases reject unsafe model IDs, but only
    // when an adapter interpolates `{model}` into a Gemini endpoint URL.
    let uses_gemini_model_template = provider.adapters.values().any(|adapter| {
        adapter.protocol == "gemini_generate_content" && adapter.endpoint.contains("{model}")
    });
    validate_catalog(provider, name, uses_gemini_model_template)?;
    validate_routes(provider, name)?;
    validate_model_aliases(provider, name, uses_gemini_model_template)?;

    Ok(())
}

/// Provider name must be non-empty and a URL-safe slug of at most 64 chars.
fn validate_name(name: &str) -> Result<(), ConfigValidationError> {
    if name.trim().is_empty() {
        return Err(ConfigValidationError::EmptyProviderName);
    }

    static PROVIDER_NAME_SLUG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[a-z0-9_-]{1,64}$").expect("valid provider name slug regex")
    });
    if !PROVIDER_NAME_SLUG.is_match(name) {
        return Err(ConfigValidationError::InvalidProviderNameFormat {
            provider: name.to_owned(),
        });
    }
    Ok(())
}

/// Checks the configured API key.
///
/// Skipped entirely for passthrough auth: the upstream credential comes from
/// the inbound client request, so the configured key may be empty or unset.
fn validate_api_key(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    if provider.passthrough_auth {
        return Ok(());
    }
    if let Some(var) = find_unresolved_env_var(provider.api_key.expose_secret()) {
        return Err(ConfigValidationError::UnresolvedEnvVar {
            provider: name.to_owned(),
            var,
        });
    }
    if provider.api_key.expose_secret().trim().is_empty() {
        return Err(ConfigValidationError::EmptyApiKey {
            provider: name.to_owned(),
        });
    }
    Ok(())
}

/// Pricing keys must be non-empty and every price must be non-negative and
/// within the per-token upper bound.
fn validate_pricing(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    for (model_id, pricing) in &provider.pricing {
        if model_id.as_str().is_empty() {
            return Err(ConfigValidationError::InvalidPricing {
                provider: name.to_owned(),
                model: model_id.as_str().to_owned(),
                message: "pricing key is empty".to_owned(),
            });
        }
        for (field, value) in [
            ("input", pricing.input),
            ("output", pricing.output),
            ("cache_creation", pricing.cache_creation),
            ("cache_read", pricing.cache_read),
            ("reasoning", pricing.reasoning),
        ] {
            if value < Decimal::ZERO {
                return Err(ConfigValidationError::InvalidPricing {
                    provider: name.to_owned(),
                    model: model_id.as_str().to_owned(),
                    message: format!("negative {field} price"),
                });
            }
            // Upper bound: every real per-token USD price is well under $1, so
            // any value above $1000/token is a gross misconfiguration (a stray
            // exponent, a per-1M-token price entered verbatim, or a misplaced
            // decimal). Rejecting it here fails fast at config-load instead of
            // letting a bad entry produce nonsensical costs. `compute_cost`
            // additionally uses checked arithmetic so even a value that slipped
            // past this bound can never panic on overflow.
            let max_price = max_per_token_price();
            if value > max_price {
                return Err(ConfigValidationError::InvalidPricing {
                    provider: name.to_owned(),
                    model: model_id.as_str().to_owned(),
                    message: format!(
                        "{field} price {value} exceeds the ${max_price}/token upper \
                         bound; per-token USD prices are expected to be far below $1 -- did you \
                         enter a per-1M-token price?"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Every adapter needs a name, a protocol, and an HTTP(S) endpoint, plus
/// header checks. Non-HTTP schemes are rejected to prevent SSRF via crafted
/// endpoints.
fn validate_adapters(
    provider: &ProviderConfig,
    name: &str,
    known_protocols: Option<&[&str]>,
) -> Result<(), ConfigValidationError> {
    for (adapter_name, adapter_cfg) in &provider.adapters {
        if adapter_name.trim().is_empty() {
            return Err(ConfigValidationError::EmptyAdapterName {
                provider: name.to_owned(),
            });
        }
        if adapter_cfg.protocol.trim().is_empty() {
            return Err(ConfigValidationError::EmptyProtocol {
                provider: name.to_owned(),
                adapter: adapter_name.clone(),
            });
        }
        if adapter_cfg.endpoint.trim().is_empty() {
            return Err(ConfigValidationError::EmptyEndpoint {
                provider: name.to_owned(),
                adapter: adapter_name.clone(),
            });
        }
        if !has_valid_http_scheme(adapter_cfg.endpoint.trim()) {
            return Err(ConfigValidationError::InvalidEndpointScheme {
                provider: name.to_owned(),
                adapter: adapter_name.clone(),
                endpoint: adapter_cfg.endpoint.clone(),
            });
        }
        if let Some(known) = known_protocols
            && !known.contains(&adapter_cfg.protocol.as_str())
        {
            return Err(ConfigValidationError::UnknownProtocol {
                provider: name.to_owned(),
                adapter: adapter_name.clone(),
                protocol: adapter_cfg.protocol.clone(),
            });
        }

        validate_headers(&adapter_cfg.headers, name, adapter_name)?;
    }
    Ok(())
}

/// Discovery, when configured, needs an HTTP(S) endpoint, valid headers, and
/// non-zero limits.
fn validate_discovery(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    let Some(discovery) = &provider.discovery else {
        return Ok(());
    };
    let endpoint = discovery.endpoint.trim();
    if endpoint.is_empty() {
        return Err(ConfigValidationError::EmptyEndpoint {
            provider: name.to_owned(),
            adapter: "discovery".to_owned(),
        });
    }
    if !has_valid_http_scheme(endpoint) {
        return Err(ConfigValidationError::InvalidEndpointScheme {
            provider: name.to_owned(),
            adapter: "discovery".to_owned(),
            endpoint: discovery.endpoint.clone(),
        });
    }
    validate_headers(&discovery.headers, name, "discovery")?;
    for (limit, value) in [
        ("max_pages", discovery.max_pages),
        ("max_models", discovery.max_models),
        ("max_response_bytes", discovery.max_response_bytes),
    ] {
        if value == 0 {
            return Err(ConfigValidationError::InvalidDiscoveryLimit {
                provider: name.to_owned(),
                limit,
            });
        }
    }
    Ok(())
}

/// Catalog entries need a non-empty ID, and a Gemini-safe one when an adapter
/// interpolates the model into its endpoint URL.
fn validate_catalog(
    provider: &ProviderConfig,
    name: &str,
    uses_gemini_model_template: bool,
) -> Result<(), ConfigValidationError> {
    let Some(catalog) = &provider.catalog else {
        return Ok(());
    };
    for entry in &catalog.models {
        if entry.id.trim().is_empty() {
            return Err(ConfigValidationError::EmptyCatalogEntryId {
                provider: name.to_owned(),
            });
        }
        if uses_gemini_model_template && !is_safe_gemini_model_id(&entry.id) {
            return Err(ConfigValidationError::UnsafeGeminiModelId {
                provider: name.to_owned(),
                location: "catalog entry",
                model: entry.id.clone(),
            });
        }
    }
    Ok(())
}

/// Each configured route must name a non-empty adapter that actually exists.
fn validate_routes(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    for (route_kind, adapter_name) in [
        (
            "chat_completions",
            provider.routes.chat_completions.as_ref(),
        ),
        ("messages", provider.routes.messages.as_ref()),
    ] {
        let Some(adapter_name) = adapter_name else {
            continue;
        };
        let aname = adapter_name.trim();
        if aname.is_empty() {
            return Err(ConfigValidationError::EmptyRouteAdapterName {
                provider: name.to_owned(),
                route_kind: route_kind.to_owned(),
            });
        }
        if !provider.adapters.contains_key(aname) {
            return Err(ConfigValidationError::UnknownRouteAdapter {
                provider: name.to_owned(),
                route_kind: route_kind.to_owned(),
                adapter: aname.to_owned(),
            });
        }
    }
    Ok(())
}

/// Alias keys and targets must be non-empty, and targets Gemini-safe when an
/// adapter interpolates the model into its endpoint URL.
fn validate_model_aliases(
    provider: &ProviderConfig,
    name: &str,
    uses_gemini_model_template: bool,
) -> Result<(), ConfigValidationError> {
    for (alias_key, alias_target) in &provider.model_aliases {
        if alias_key.trim().is_empty() {
            return Err(ConfigValidationError::EmptyModelAliasKey {
                provider: name.to_owned(),
            });
        }
        if alias_target.trim().is_empty() {
            return Err(ConfigValidationError::EmptyModelAliasTarget {
                provider: name.to_owned(),
                alias: alias_key.clone(),
            });
        }
        if uses_gemini_model_template && !is_safe_gemini_model_id(alias_target) {
            return Err(ConfigValidationError::UnsafeGeminiModelId {
                provider: name.to_owned(),
                location: "model alias target",
                model: alias_target.clone(),
            });
        }
    }
    Ok(())
}

/// Forbidden HTTP header names that providers must not set via static headers.
///
/// These headers are managed by the HTTP transport layer and must not be
/// overridden by provider configuration, as doing so could break request
/// routing, authentication, or connection handling.
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "upgrade",
    "proxy-authorization",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
];

/// Check whether an endpoint URL has a valid HTTP(S) scheme.
///
/// Rejects non-HTTP schemes (e.g. `file://`, `ftp://`) to prevent SSRF
/// attacks via crafted endpoint URLs.
fn has_valid_http_scheme(endpoint: &str) -> bool {
    // Schemes are case-insensitive per RFC 3986 (e.g. `HTTP://`, `Https://`),
    // and reqwest accepts them. Lowercase only the leading scheme region
    // (everything up to the first `://`, capped at the known scheme length) so
    // the prefix test is case-insensitive without allocating a full lowercase
    // copy of the endpoint or pulling the `url` crate into llm-proxy-core.
    let scheme_region = scheme_region(endpoint);
    scheme_region.eq_ignore_ascii_case("http") || scheme_region.eq_ignore_ascii_case("https")
}

/// Return the leading scheme region of `endpoint` (the text before the first
/// `://`, or the empty string when no `://` separator is present).
fn scheme_region(endpoint: &str) -> &str {
    match endpoint.find("://") {
        Some(idx) => &endpoint[..idx],
        None => "",
    }
}

/// Validate a map of static headers for CRLF injection and forbidden header names.
///
/// Shared between adapter header validation and discovery header validation
/// to avoid duplicating the same checks.
fn validate_headers(
    headers: &HashMap<String, String>,
    provider: &str,
    adapter: &str,
) -> Result<(), ConfigValidationError> {
    for (hname, hvalue) in headers {
        if hname.trim().is_empty() {
            return Err(ConfigValidationError::EmptyHeaderName {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
            });
        }
        if hname.contains('\r')
            || hname.contains('\n')
            || hvalue.contains('\r')
            || hvalue.contains('\n')
        {
            return Err(ConfigValidationError::HeaderContainsCrlf {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
            });
        }
        let lower = hname.to_ascii_lowercase();
        if FORBIDDEN_HEADERS.contains(&lower.as_str()) {
            return Err(ConfigValidationError::ForbiddenHeader {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
                header: hname.clone(),
            });
        }
    }
    Ok(())
}

fn is_safe_gemini_model_id(model: &str) -> bool {
    let component = model.strip_prefix("models/").unwrap_or(model);
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

// ---------------------------------------------------------------------------
// TOML loading
// ---------------------------------------------------------------------------

/// Load and parse a main [`AppConfig`] TOML file.
///
/// The raw file content is first processed for `${ENV_VAR}` interpolation,
/// then parsed as TOML, then validated.
///
/// # Blocking I/O
///
/// This function performs synchronous filesystem I/O and is intended for use
/// at startup. If hot-reload is implemented in future, an async variant will
/// be needed.
///
/// # Trust boundary
///
/// The `path` parameter accepts arbitrary filesystem paths without
/// canonicalisation or path-traversal checks. This is standard for
/// env-var/CLI-driven config loading -- the process owner controls the path.
/// The config file content is treated as trusted input.
///
/// # Errors
///
/// Returns a [`CoreError`] if the file cannot be read, the TOML is malformed,
/// env-var interpolation encounters issues, or route validation fails.
pub fn load_app_config(path: impl AsRef<Path>) -> Result<AppConfig, CoreError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|source| CoreError::ConfigLoad {
        path: path.to_path_buf(),
        source,
    })?;

    // Check for env vars that resolve to empty, consistent with provider config.
    for (var_name, resolved) in find_env_var_refs(&raw) {
        if let Some(value) = resolved {
            if value.is_empty() {
                return Err(CoreError::ConfigValidation {
                    message: format!(
                        "app config environment variable \"{var_name}\" resolved to an empty value"
                    ),
                    source: None,
                });
            }
        }
    }

    let interpolated = interpolate_env_vars(&raw);

    // After interpolation, check for any unresolved env vars (set but not
    // present in the environment). This ensures `${MISSING_VAR}` in server
    // fields like `bind` or `server_name` is caught rather than silently
    // passed through as a literal string.
    if let Some(var) = find_unresolved_env_var(&interpolated) {
        return Err(CoreError::ConfigValidation {
            message: format!("app config contains unresolvable environment variable \"{var}\""),
            source: None,
        });
    }

    let cfg: AppConfig = toml::from_str(&interpolated).map_err(CoreError::ConfigParse)?;
    if cfg.server.hot_reload {
        return Err(CoreError::ConfigValidation {
            message:
                "server.hot_reload=true is not supported; restart the server after config changes"
                    .to_owned(),
            source: None,
        });
    }
    // Reject a zero or sub-second request timeout. tower-http's `TimeoutLayer`
    // builds `tokio::time::sleep(self.timeout)` on every call; `sleep(ZERO)` is
    // immediately ready, so `request_timeout = "0s"` would return 408 for every
    // API request before any handler runs. A sub-second timeout is almost
    // always a misconfiguration (e.g. mistyped units) and silently bricks the
    // API the same way. Require at least one second. (HIGH-2 / LOW-10.)
    const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
    if cfg.server.request_timeout < MIN_REQUEST_TIMEOUT {
        return Err(CoreError::ConfigValidation {
            message: format!(
                "server.request_timeout must be at least 1 second, but is {:?}; \
                 a zero or sub-second timeout makes every API request return 408 \
                 before any handler runs",
                cfg.server.request_timeout
            ),
            source: None,
        });
    }
    // Reject a dedup window whose millisecond magnitude does not fit u64. The
    // deduplicator stores the window as a `u64` millisecond count (see
    // `AppState::new_with_catalog_dir` in llm-proxy-server), so an out-of-range
    // `Duration` would otherwise silently fall back to the `unwrap_or` default
    // rather than being reported. `Duration::MAX` is the only realistic value
    // that trips this, and it indicates a misconfiguration.
    if u64::try_from(cfg.server.dedup_window.as_millis()).is_err() {
        return Err(CoreError::ConfigValidation {
            message: format!(
                "server.dedup_window is {:?}, whose millisecond magnitude does \
                 not fit u64; use a smaller window (0s disables deduplication)",
                cfg.server.dedup_window
            ),
            source: None,
        });
    }
    validate_auth_config(&cfg.auth, cfg.server.bind)?;
    Ok(cfg)
}

/// Reserved caller name used for unauthenticated requests under
/// [`AuthMode::None`]; a client key id must not collide with it or event
/// attribution becomes ambiguous.
pub const ANONYMOUS_CALLER: &str = "anonymous";

/// Validate `[auth]` against itself and against the bind address.
///
/// Rules:
/// - `mode = "none"` with a non-empty `[auth.clients]` is contradictory
///   (configured keys would be silently ignored) and is rejected.
/// - `mode = "client-keys"` requires at least one `[auth.clients]` entry.
/// - `mode = "none"` is only permitted on a loopback bind: a non-loopback
///   bind with no client keys would expose the proxy to every reachable
///   peer with no authentication, so startup refuses.
/// - Key ids must be non-empty and must not be [`ANONYMOUS_CALLER`].
fn validate_auth_config(auth: &AuthConfig, bind: SocketAddr) -> Result<(), CoreError> {
    let clients_empty = auth.clients.is_empty();
    match auth.mode {
        AuthMode::None if !clients_empty => {
            return Err(CoreError::ConfigValidation {
                message: "auth.mode = \"none\" but [auth.clients] is non-empty; \
                          set auth.mode = \"client-keys\" to enforce the configured keys"
                    .to_owned(),
                source: None,
            });
        }
        AuthMode::ClientKeys if clients_empty => {
            return Err(CoreError::ConfigValidation {
                message: "auth.mode = \"client-keys\" requires at least one entry \
                          under [auth.clients]"
                    .to_owned(),
                source: None,
            });
        }
        AuthMode::None if !bind.ip().is_loopback() => {
            return Err(CoreError::ConfigValidation {
                message: format!(
                    "server.bind {bind} is not loopback and no client keys are \
                     configured; anonymous auth is only permitted on loopback binds. \
                     Configure [auth.clients] and set auth.mode = \"client-keys\""
                ),
                source: None,
            });
        }
        AuthMode::None | AuthMode::ClientKeys => {}
    }
    for key_id in auth.clients.keys() {
        if key_id.is_empty() {
            return Err(CoreError::ConfigValidation {
                message: "[auth.clients] contains an empty key id".to_owned(),
                source: None,
            });
        }
        if key_id == ANONYMOUS_CALLER {
            return Err(CoreError::ConfigValidation {
                message: format!(
                    "[auth.clients] key id \"{ANONYMOUS_CALLER}\" is reserved for \
                     unattributed requests; choose a different key id"
                ),
                source: None,
            });
        }
    }
    Ok(())
}

/// Load and parse a provider [`ProviderFile`] TOML file.
///
/// The raw file content is first processed for `${ENV_VAR}` interpolation,
/// then parsed as TOML, then validated.
///
/// `known_protocols` is forwarded to [`validate_provider_config`].
///
/// # Blocking I/O
///
/// This function performs synchronous filesystem I/O and is intended for use
/// at startup. If hot-reload is implemented in future, an async variant will
/// be needed.
///
/// # Trust boundary
///
/// The `path` parameter accepts arbitrary filesystem paths without
/// canonicalisation or path-traversal checks. This is standard for
/// env-var/CLI-driven config loading -- the process owner controls the path.
/// The config file content is treated as trusted input.
///
/// # Errors
///
/// Returns a [`CoreError`] if the file cannot be read, the TOML is malformed,
/// an env var referenced in the file resolves to an empty value, or validation
/// of the parsed [`ProviderConfig`] fails.
pub fn load_provider_config(
    path: impl AsRef<Path>,
    known_protocols: Option<&[&str]>,
) -> Result<ProviderConfig, CoreError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|source| CoreError::ConfigLoad {
        path: path.to_path_buf(),
        source,
    })?;

    // Before interpolation, check if any env vars referenced in the raw TOML
    // resolve to empty strings. This must happen before interpolation replaces
    // the ${VAR} pattern, otherwise we lose the diagnostic info about which
    // env var was empty.
    //
    // WHY: We scan the raw TOML string with the shared env-var regex instead of
    // parsing the TOML twice. This avoids a redundant full TOML parse just to
    // extract the api_key value for the empty-env-var check.
    for (var_name, resolved) in find_env_var_refs(&raw) {
        match resolved {
            None => {
                // Env var is not set -- interpolation will leave ${VAR} as-is.
                // The unresolved-env-var check below (after interpolation) will
                // catch this for all fields, not just api_key.
            }
            Some(value) if value.is_empty() => {
                // Provider name is not yet known (no parse has occurred), so we
                // include the file path as context instead. Carried as a typed
                // `CoreError::ConfigValidation` source via `From` (GAP-LOW-11),
                // so this failure is variant-recoverable by callers.
                return Err(ConfigValidationError::EmptyEnvVar {
                    provider: format!("(file: {})", path.display()),
                    var: var_name,
                }
                .into());
            }
            Some(_) => {}
        }
    }

    // Now perform full interpolation on the raw content and check for any
    // unresolved env vars that remain. This catches ${MISSING_VAR} patterns
    // in *any* field (endpoint, protocol, name), not just api_key.
    // validate_provider_config only checks api_key for unresolved patterns,
    // so a whole-file check is needed here to match load_app_config behavior.
    let interpolated = interpolate_env_vars(&raw);
    if let Some(var) = find_unresolved_env_var(&interpolated) {
        return Err(CoreError::ConfigValidation {
            message: format!(
                "provider config contains unresolvable environment variable \"{var}\""
            ),
            source: None,
        });
    }
    let file: ProviderFile = toml::from_str(&interpolated).map_err(CoreError::ConfigParse)?;
    // The typed `ConfigValidationError` flows into `CoreError::ConfigValidation`
    // as a first-class typed source via `From` (GAP-LOW-11), not boxed behind
    // `dyn Error`, so callers can match on the concrete variant.
    validate_provider_config(&file.provider, known_protocols).map_err(CoreError::from)?;
    Ok(file.provider)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_rejects_removed_models_table() {
        let result = toml::from_str::<AppConfig>(
            "[server]\nbind = '127.0.0.1:3456'\n[models]\nfoo = { provider = 'bar' }\n",
        );
        result.unwrap_err();
    }

    #[test]
    fn app_config_defaults_operational_controls() {
        let config: AppConfig = toml::from_str(
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .expect("app config");

        assert_eq!(config.server.rate_limit_rpm, 100);
        assert!(!config.server.trust_forwarded_headers);
        assert_eq!(config.server.dedup_window, Duration::ZERO);
        // shutdown_timeout omits from the TOML above and must default to 30s (HIGH-1).
        assert_eq!(config.server.shutdown_timeout, Duration::from_secs(30));
    }

    #[test]
    fn app_config_accepts_explicit_shutdown_timeout() {
        // Operators may override the 30s default; the value round-trips verbatim.
        let config: AppConfig = toml::from_str(
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
shutdown_timeout = "10s"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .expect("app config");
        assert_eq!(config.server.shutdown_timeout, Duration::from_secs(10));
    }

    #[test]
    fn load_app_config_rejects_enabled_hot_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = true
server_name = "test"
"#,
        )
        .unwrap();

        let error = load_app_config(&path).expect_err("hot reload is unsupported");

        assert!(
            error
                .to_string()
                .contains("hot_reload=true is not supported")
        );
    }

    #[test]
    fn load_app_config_rejects_zero_request_timeout() {
        // HIGH-2: a zero timeout makes every API request 408 before any handler.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "0s"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .unwrap();

        let error = load_app_config(&path).expect_err("zero timeout must be rejected");
        assert!(
            error
                .to_string()
                .contains("server.request_timeout must be at least 1 second"),
            "got: {error}"
        );
    }

    #[test]
    fn load_app_config_rejects_subsecond_request_timeout() {
        // Sub-second timeouts are almost always a mistyped unit and brick the API.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "500ms"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .unwrap();

        let error = load_app_config(&path).expect_err("sub-second timeout must be rejected");
        assert!(
            error
                .to_string()
                .contains("server.request_timeout must be at least 1 second"),
            "got: {error}"
        );
    }

    #[test]
    fn load_app_config_rejects_dedup_window_exceeding_u64_millis() {
        // A dedup window whose millisecond magnitude does not fit u64 (only
        // realistically `Duration::MAX`) cannot be stored by the deduplicator
        // and must be rejected at load time rather than silently clamped.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
dedup_window = "5000000000000000000s"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .unwrap();

        let error = load_app_config(&path).expect_err("oversized dedup window must be rejected");
        assert!(
            error.to_string().contains("server.dedup_window")
                && error.to_string().contains("does not fit u64"),
            "got: {error}"
        );
    }

    /// Write a config file with the given `bind` and `[auth]` body into a
    /// fresh temp dir, returning the dir (the config lives at
    /// `config.toml` inside it). The returned guard keeps the file alive;
    /// callers run [`load_app_config`] on it directly.
    fn config_dir_with_auth(bind: &str, auth_body: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[server]
bind = "{bind}"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"

{auth_body}
"#
            ),
        )
        .expect("write config file");
        directory
    }

    #[test]
    fn auth_absent_section_defaults_to_none_on_loopback() {
        let dir = config_dir_with_auth("127.0.0.1:3456", "");
        let config =
            load_app_config(dir.path().join("config.toml")).expect("default auth accepted");
        assert_eq!(config.auth.mode, AuthMode::None);
        assert!(config.auth.clients.is_empty());
    }

    #[test]
    fn auth_none_on_non_loopback_bind_is_rejected() {
        // The core safety rule of client identity: an anonymous proxy exposed
        // past loopback must refuse to start, before any listener binds.
        for bind in ["0.0.0.0:3456", "192.168.1.4:3456", "[::]:3456"] {
            let dir = config_dir_with_auth(bind, "");
            let error = load_app_config(dir.path().join("config.toml"))
                .expect_err("non-loopback none must be rejected");
            assert!(
                error.to_string().contains("not loopback"),
                "bind {bind}: got: {error}"
            );
        }
    }

    #[test]
    fn auth_none_on_ipv6_loopback_bind_is_accepted() {
        // `::1` is loopback; the loopback exemption is about the address, not
        // the address family.
        let dir = config_dir_with_auth("[::1]:3456", "");
        load_app_config(dir.path().join("config.toml")).expect("ipv6 loopback accepted");
    }

    #[test]
    fn auth_none_with_configured_clients_is_rejected() {
        let dir = config_dir_with_auth(
            "127.0.0.1:3456",
            r#"
[auth]
mode = "none"

[auth.clients]
laptop = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$hash"
"#,
        );
        let error = load_app_config(dir.path().join("config.toml"))
            .expect_err("none mode with clients is contradictory");
        assert!(
            error.to_string().contains("[auth.clients] is non-empty"),
            "got: {error}"
        );
    }

    #[test]
    fn auth_client_keys_without_clients_is_rejected() {
        let dir = config_dir_with_auth(
            "0.0.0.0:3456",
            r#"
[auth]
mode = "client-keys"
"#,
        );
        let error = load_app_config(dir.path().join("config.toml"))
            .expect_err("client-keys mode needs at least one key");
        assert!(
            error
                .to_string()
                .contains("requires at least one entry under [auth.clients]"),
            "got: {error}"
        );
    }

    #[test]
    fn auth_client_keys_with_a_client_accepts_any_bind() {
        // With at least one hashed key configured, a non-loopback bind is a
        // legitimate deployment and must load.
        let dir = config_dir_with_auth(
            "0.0.0.0:3456",
            r#"
[auth]
mode = "client-keys"

[auth.clients]
laptop = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$hash"
"#,
        );
        let config = load_app_config(dir.path().join("config.toml"))
            .expect("client-keys on non-loopback accepted");
        assert_eq!(config.auth.mode, AuthMode::ClientKeys);
        assert_eq!(config.auth.clients.len(), 1);
    }

    #[test]
    fn auth_rejects_reserved_and_empty_key_ids() {
        let reserved_dir = config_dir_with_auth(
            "127.0.0.1:3456",
            r#"
[auth]
mode = "client-keys"

[auth.clients]
anonymous = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$hash"
"#,
        );
        let reserved = load_app_config(reserved_dir.path().join("config.toml"))
            .expect_err("reserved key id must be rejected");
        assert!(reserved.to_string().contains("reserved"), "got: {reserved}");

        let empty_dir = config_dir_with_auth(
            "127.0.0.1:3456",
            r#"
[auth]
mode = "client-keys"

[auth.clients]
"" = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$hash"
"#,
        );
        let empty = load_app_config(empty_dir.path().join("config.toml"))
            .expect_err("empty key id must be rejected");
        assert!(empty.to_string().contains("empty key id"), "got: {empty}");
    }

    #[test]
    fn auth_clients_do_not_leak_through_debug() {
        // The hash (let alone a plaintext secret) must never surface in Debug
        // output of the config, matching the api_key discipline.
        let config: AppConfig = toml::from_str(
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"

[auth]
mode = "client-keys"

[auth.clients]
laptop = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$hashvalue"
"#,
        )
        .expect("app config");
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("hashvalue"),
            "client hash must be redacted in Debug, got: {debug}"
        );
        assert!(debug.contains("[REDACTED]"), "got: {debug}");
    }

    #[test]
    fn has_valid_http_scheme_is_case_insensitive() {
        // Schemes are case-insensitive per RFC 3986 and reqwest accepts mixed
        // case; the validator must not reject `HTTP://` / `Https://`.
        assert!(has_valid_http_scheme("http://host"));
        assert!(has_valid_http_scheme("https://host"));
        assert!(has_valid_http_scheme("HTTP://host"));
        assert!(has_valid_http_scheme("Https://host"));
        assert!(has_valid_http_scheme("HtTp://host/path"));
        // Non-HTTP schemes are still rejected.
        assert!(!has_valid_http_scheme("file://host"));
        assert!(!has_valid_http_scheme("ftp://host"));
        assert!(!has_valid_http_scheme("host"));
    }

    #[test]
    fn provider_routes_rejects_unknown_fields() {
        let result = toml::from_str::<ProviderFile>(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.routes]
chat_completion = "chat"
"#,
        );

        result.unwrap_err();
    }

    #[test]
    fn provider_debug_redacts_header_values_and_endpoint_query() {
        let config: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "secret-api-key"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions?key=adapter-query-secret"

[provider.adapters.chat.headers]
x-secret = "adapter-secret"

[provider.routes]
chat_completions = "chat"

[provider.discovery]
kind = "openai_compatible_models"
endpoint = "https://example.com/v1/models?key=query-secret"

[provider.discovery.headers]
x-discovery-secret = "super-sensitive-value"
"#,
        )
        .expect("provider config");
        let debug = format!("{:?}", config.provider);
        assert!(!debug.contains("secret-api-key"));
        assert!(!debug.contains("adapter-secret"));
        assert!(!debug.contains("adapter-query-secret"));
        assert!(!debug.contains("super-sensitive-value"));
        assert!(!debug.contains("query-secret"));
    }

    #[test]
    fn env_interpolated_api_key_loads_into_secret_string() {
        // Production secret-loading path: `${VAR}` in the raw TOML is resolved
        // by env interpolation *before* serde constructs the `SecretString`, so
        // the resolved value is what gets wrapped and `expose_secret()` returns.
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _guard =
            crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_SECRET_KEY", "sk-resolved-007");
        let raw = r#"
[provider]
name = "test"
api_key = "${_LLM_PROXY_TEST_SECRET_KEY}"
auth_style = "bearer"
"#;
        let interpolated = interpolate_env_vars(raw);
        let file: ProviderFile = toml::from_str(&interpolated).expect("provider parses");
        let key: &str = file.provider.api_key.expose_secret();
        assert_eq!(
            key, "sk-resolved-007",
            "env-interpolated value must land in the SecretString",
        );
    }

    #[test]
    fn discovery_rejects_transport_headers() {
        let mut provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.discovery]
kind = "openai_compatible_models"
endpoint = "https://example.com/v1/models"
"#,
        )
        .expect("provider config");
        provider
            .provider
            .discovery
            .as_mut()
            .expect("discovery")
            .headers
            .insert("host".to_owned(), "evil.example".to_owned());
        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::ForbiddenHeader { .. })
        ));
    }

    #[test]
    fn discovery_limits_use_secure_defaults() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.discovery]
kind = "openai_compatible_models"
endpoint = "https://example.com/v1/models"
"#,
        )
        .expect("provider config");
        let discovery = provider.provider.discovery.expect("discovery");

        assert_eq!(
            (
                discovery.max_pages,
                discovery.max_models,
                discovery.max_response_bytes,
            ),
            (100, 20_000, 4 * 1024 * 1024)
        );
    }

    #[test]
    fn discovery_rejects_zero_limits() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.discovery]
kind = "openai_compatible_models"
endpoint = "https://example.com/v1/models"
max_pages = 0
"#,
        )
        .expect("provider config");

        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::InvalidDiscoveryLimit {
                limit: "max_pages",
                ..
            })
        ));
    }

    #[test]
    fn gemini_alias_target_rejects_unsafe_url_template_characters() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "gemini"
api_key = "key"
auth_style = "x-google-api-key"

[provider.adapters.generate]
protocol = "gemini_generate_content"
endpoint = "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"

[provider.routes]
messages = "generate"

[provider.model_aliases]
unsafe = "foo/../bar"
"#,
        )
        .expect("provider config");

        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::UnsafeGeminiModelId {
                location: "model alias target",
                ..
            })
        ));
    }

    #[test]
    fn gemini_static_catalog_entry_rejects_unsafe_url_template_characters() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "gemini"
api_key = "key"
auth_style = "x-google-api-key"

[provider.adapters.generate]
protocol = "gemini_generate_content"
endpoint = "https://generativelanguage.googleapis.com/v1beta/{model}:generateContent"

[provider.routes]
chat_completions = "generate"

[[provider.catalog.models]]
id = "models/group/gemini-pro"
"#,
        )
        .expect("provider config");

        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::UnsafeGeminiModelId {
                location: "catalog entry",
                ..
            })
        ));
    }

    #[test]
    fn gemini_resource_model_id_is_valid_at_config_load() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "gemini"
api_key = "key"
auth_style = "x-google-api-key"

[provider.adapters.generate]
protocol = "gemini_generate_content"
endpoint = "https://generativelanguage.googleapis.com/v1beta/{model}:generateContent"

[provider.routes]
chat_completions = "generate"

[provider.model_aliases]
pro = "models/gemini-2.5-pro"

[[provider.catalog.models]]
id = "models/gemini-2.5-pro"
"#,
        )
        .expect("provider config");

        validate_provider_config(&provider.provider, None).unwrap();
    }

    // -- Adapter header CRLF validation ---------------------------------------

    #[test]
    fn adapter_rejects_header_with_crlf() {
        let mut provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#,
        )
        .expect("provider config");
        let headers = &mut provider.provider.adapters.get_mut("chat").unwrap().headers;
        Arc::make_mut(headers).insert("x-evil".to_owned(), "value\r\nInjected: true".to_owned());
        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::HeaderContainsCrlf { .. })
        ));
    }

    #[test]
    fn discovery_rejects_header_with_crlf() {
        let mut provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.discovery]
kind = "openai_compatible_models"
endpoint = "https://example.com/v1/models"
"#,
        )
        .expect("provider config");
        provider
            .provider
            .discovery
            .as_mut()
            .unwrap()
            .headers
            .insert("x-evil".to_owned(), "val\nInjected: true".to_owned());
        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::HeaderContainsCrlf { .. })
        ));
    }

    // -- Empty adapter name, protocol, model alias key/target -----------------

    #[test]
    fn validate_rejects_empty_adapter_name() {
        let provider: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.adapters."" ]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1"
"#,
        )
        .expect("provider config");
        // The TOML key "" is deserialized as an empty string.
        assert!(matches!(
            validate_provider_config(&provider.provider, None),
            Err(ConfigValidationError::EmptyAdapterName { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_protocol() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: String::new(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                },
            )]),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::EmptyProtocol { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_model_alias_key() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                },
            )]),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::from([("".to_owned(), "target".to_owned())]),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::EmptyModelAliasKey { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_model_alias_target() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                },
            )]),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::from([("alias".to_owned(), "".to_owned())]),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::EmptyModelAliasTarget { .. })
        ));
    }

    #[test]
    fn validate_rejects_provider_name_exceeding_length_limit() {
        let provider = ProviderConfig {
            name: "a".repeat(65), // 65 chars exceeds the 64-char limit
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::InvalidProviderNameFormat { .. })
        ));
    }

    #[test]
    fn validate_accepts_provider_name_at_max_length() {
        let provider = ProviderConfig {
            name: "a".repeat(64), // exactly 64 chars
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        // Should not fail on name validation (will fail on empty api_key
        // if key is empty, but our key is "key").
        // Actually no adapters/routes means it passes.
        validate_provider_config(&provider, None).unwrap();
    }

    // -- Pricing / cost ------------------------------------------------------

    #[test]
    fn model_pricing_parses_from_toml_with_quoted_decimals() {
        let raw = r#"
[provider]
name = "test"
api_key = "key"
auth_style = "bearer"

[provider.pricing."gpt-4o"]
input = "0.0000025"
output = "0.000010"
cache_creation = "0.000003"
cache_read = "0.00000125"
reasoning = "0.000010"
"#;
        let file: ProviderFile = toml::from_str(raw).expect("parses");
        let pricing = file
            .provider
            .pricing
            .get(&ModelId::new("gpt-4o"))
            .expect("pricing for gpt-4o");
        assert_eq!(pricing.input.to_string(), "0.0000025");
        assert_eq!(pricing.output.to_string(), "0.000010");
        assert_eq!(pricing.cache_creation.to_string(), "0.000003");
        assert_eq!(pricing.cache_read.to_string(), "0.00000125");
        assert_eq!(pricing.reasoning.to_string(), "0.000010");
    }

    #[test]
    fn model_pricing_unquoted_float_is_rejected() {
        // An unquoted decimal is parsed by TOML as f64; rust_decimal rejects
        // floats, so deserialization fails. Operators MUST quote decimals.
        let raw = r#"
[provider]
name = "test"
api_key = "key"
auth_style = "bearer"
[provider.pricing."gpt-4o"]
input = 0.0000025
"#;
        toml::from_str::<ProviderFile>(raw).unwrap_err();
    }

    #[test]
    fn validate_rejects_negative_pricing() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::from([(
                ModelId::new("gpt-4o"),
                ModelPricing {
                    input: "-0.0001".parse().unwrap(),
                    ..ModelPricing::default()
                },
            )]),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::InvalidPricing { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_pricing_model_id() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::from([(ModelId::new(""), ModelPricing::default())]),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::InvalidPricing { .. })
        ));
    }

    #[test]
    fn validate_rejects_implausibly_large_pricing() {
        // A per-token USD price above the MAX_PER_TOKEN_PRICE bound is a gross
        // misconfiguration (a stray exponent, a per-1M-token price entered
        // verbatim, or a misplaced decimal) and is rejected at load time rather
        // than allowed to produce nonsensical costs.
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::from([(
                ModelId::new("gpt-4o"),
                // $1000000/token: parses cleanly as a Decimal but exceeds the bound.
                ModelPricing {
                    input: "1000000".parse().unwrap(),
                    ..ModelPricing::default()
                },
            )]),
        };
        match validate_provider_config(&provider, None) {
            Err(ConfigValidationError::InvalidPricing { message, .. }) => {
                assert!(
                    message.contains("upper") || message.contains("bound"),
                    "expected an upper-bound message, got: {message}"
                );
            }
            other => panic!("expected InvalidPricing for a huge price, got: {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_realistic_pricing_below_bound() {
        // A realistic per-token price (a few $/1M tokens -> tiny $/token) is
        // accepted. Guards against the upper bound being too tight.
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: HashMap::from([(
                ModelId::new("gpt-4o"),
                ModelPricing {
                    input: "0.0000025".parse().unwrap(),
                    output: "0.000010".parse().unwrap(),
                    ..ModelPricing::default()
                },
            )]),
        };
        validate_provider_config(&provider, None).unwrap();
    }

    #[test]
    fn log_format_from_env_reads_rust_log_format() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        // "json" (case-insensitive) -> Json.
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "JSON");
            assert_eq!(LogFormat::from_env(), LogFormat::Json);
        }
        // Unrecognized value -> Plain.
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "xml");
            assert_eq!(LogFormat::from_env(), LogFormat::Plain);
        }
        // Empty string -> Plain (same as unset).
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "");
            assert_eq!(LogFormat::from_env(), LogFormat::Plain);
        }
    }

    #[test]
    fn validate_accepts_empty_api_key_when_passthrough_auth() {
        // A passthrough-auth provider gets its upstream credential from the
        // inbound client request, so an empty/unset api_key must be allowed.
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from(""),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: true,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(
            validate_provider_config(&provider, None).is_ok(),
            "passthrough provider with empty api_key must pass validation"
        );
    }

    #[test]
    fn validate_rejects_empty_api_key_when_not_passthrough() {
        // Non-passthrough providers still require a non-empty api_key.
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from(""),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::EmptyApiKey { .. })
        ));
    }
}
