//! TOML provider configuration types, parsing, env-var interpolation, and
//! validation. The main `config.toml` holds server settings; provider files
//! hold provider-scoped routing and catalog configuration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::env_interpolate::{find_env_var_refs, find_unresolved_env_var, interpolate_env_vars};
use crate::error::CoreError;

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
    /// Usage-ledger persistence settings (`[storage]` section). Absent keeps
    /// the in-memory no-op event bus (nothing is persisted).
    #[serde(default)]
    pub storage: Option<StorageConfig>,
}

/// Named security postures for where the admin `/usage` API is reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminMountPosture {
    /// Default: `/usage` is served only on the dedicated
    /// [`ServerConfig::admin_bind`] listener (loopback unless configured
    /// otherwise); the main listener has no admin surface at all.
    DedicatedListener,
    /// Opt-in (`server.admin_on_main_listener = true`): `/usage` is also
    /// served on the main `bind` listener behind the same admin token, so
    /// the admin surface reaches every network the main listener does.
    MainListener,
}

impl AdminMountPosture {
    /// Stable name for this posture, used in startup logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DedicatedListener => "dedicated-admin-listener",
            Self::MainListener => "admin-on-main-listener",
        }
    }
}

impl AppConfig {
    /// The admin `/usage` mount posture, or `None` when no
    /// `[auth.admin]` token is configured (no admin surface anywhere).
    #[must_use]
    pub fn admin_posture(&self) -> Option<AdminMountPosture> {
        let posture = if self.server.admin_on_main_listener {
            AdminMountPosture::MainListener
        } else {
            AdminMountPosture::DedicatedListener
        };
        self.auth.admin.as_ref().map(|_| posture)
    }
}

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
    /// subscriber init, before config load.
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
    /// connections and waits this long for in-flight requests to drain, then
    /// force-exits so orchestrators can recycle the process. `0s` disables
    /// the hard deadline (drain forever) but is strongly discouraged.
    #[serde(default = "default_shutdown_timeout", with = "humantime_serde")]
    pub shutdown_timeout: Duration,
    /// Log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
    pub log_level: String,
    /// Log output format for the tracing subscriber: `"plain"` (default) or
    /// `"json"`. The effective format is read from `RUST_LOG_FORMAT` at
    /// subscriber init (see [`LogFormat::from_env`]); this field documents intent.
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
    /// Cross-origin origins permitted to call the proxy from a browser.
    ///
    /// `None` (the default) installs **no** CORS layer, so browsers enforce a
    /// same-origin policy and this server-to-server proxy is not unexpectedly
    /// reachable from arbitrary web origins. Setting a list installs a
    /// restrictive `CorsLayer` that echoes `Access-Control-Allow-Origin` only
    /// for these exact origins.
    ///
    /// Each entry should be an absolute origin (`scheme://host[:port]`),
    /// e.g. `"https://app.example.com"`. Entries that fail to parse as a
    /// header value are skipped (with a warning) when the router is built.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// Dedicated listener bind for the admin `/usage` API. Only read when
    /// `[auth.admin]` is configured; the default is `127.0.0.1` on the port
    /// after `bind`'s port (see [`ServerConfig::effective_admin_bind`]).
    #[serde(default)]
    pub admin_bind: Option<SocketAddr>,
    /// Also serve `/usage` on the main listener behind the same admin token.
    ///
    /// Opt-in; selects the [`AdminMountPosture::MainListener`] posture, which
    /// the serve path announces with a loud startup warning: the admin
    /// surface then reaches every network the main listener does. With no
    /// explicit `admin_bind` this posture serves `/usage` on the main
    /// listener ONLY — no dedicated socket is attempted.
    #[serde(default)]
    pub admin_on_main_listener: bool,
    /// Per-endpoint-family overrides (audit conflict 16): a chat family and
    /// an audio/file family do not share one body limit or one timeout. Keys
    /// are family names (`"messages"`, `"chat_completions"`, `"models"`,
    /// `"count_tokens"`); absent families use the server-wide defaults.
    #[serde(default)]
    pub endpoint_families: std::collections::HashMap<String, EndpointFamilyConfig>,
    /// Phase-shaped upstream timeouts (backlog 8, BUG-14): connect,
    /// first-byte, in-stream idle and an optional one-deadline-per-request
    /// total, validated at config load.
    #[serde(default)]
    pub upstream_timeouts: UpstreamTimeoutsConfig,
    /// Bare `/v1/*` routing (plan 04 step 11): off by default; when enabled
    /// a model name resolves to a provider or pool.
    #[serde(default)]
    pub bare_v1: BareV1Config,
}

/// Per-endpoint-family settings (audit conflict 16): the body limit and
/// request timeout one family applies, independent of other families.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointFamilyConfig {
    /// Maximum request body this family accepts, in bytes.
    pub max_body_bytes: usize,
    /// Maximum request duration this family applies.
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
}

/// Phase-shaped upstream timeouts (backlog 8, BUG-14): one set of phases for
/// every upstream request, replacing the fixed 60-second headers deadline.
/// Cross-validated at config load ([`validate_upstream_timeouts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTimeoutsConfig {
    /// TCP/TLS connect deadline per upstream attempt.
    #[serde(default = "default_connect_timeout", with = "humantime_serde")]
    pub connect: Duration,
    /// Time-to-first-byte deadline: waiting for response headers on a
    /// non-streaming request, or for the first stream event on a streaming
    /// request. Does not cap the body.
    #[serde(default = "default_first_byte_timeout", with = "humantime_serde")]
    pub first_byte: Duration,
    /// In-stream idle deadline: a stream silent for longer than this between
    /// chunks is a dead upstream.
    #[serde(default = "default_idle_timeout", with = "humantime_serde")]
    pub idle: Duration,
    /// One deadline across every attempt of one request (retry ladder
    /// included). `None` (the default) leaves the ladder unbounded here; the
    /// per-route `[provider.retry]` / `[pool.retry]` `total_timeout` may still
    /// bound it.
    #[serde(default, with = "humantime_serde::option")]
    pub total: Option<Duration>,
}

const fn default_connect_timeout() -> Duration {
    Duration::from_secs(10)
}

const fn default_first_byte_timeout() -> Duration {
    Duration::from_secs(60)
}

const fn default_idle_timeout() -> Duration {
    Duration::from_secs(60)
}

impl Default for UpstreamTimeoutsConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            total: None,
        }
    }
}

/// Validates one [`UpstreamTimeoutsConfig`]: every phase is positive, connect
/// fits inside first-byte, and the total deadline (when set) covers
/// first-byte. Called at config load and by builders.
///
/// # Errors
///
/// A [`ConfigValidationError::InvalidTimeouts`] naming the first broken rule.
pub fn validate_upstream_timeouts(
    timeouts: &UpstreamTimeoutsConfig,
) -> Result<(), ConfigValidationError> {
    let phases = [
        ("connect", timeouts.connect),
        ("first_byte", timeouts.first_byte),
        ("idle", timeouts.idle),
    ];
    for &(name, value) in &phases {
        if value.is_zero() {
            return Err(ConfigValidationError::InvalidTimeouts {
                message: format!("upstream timeout phase \"{name}\" must be positive"),
            });
        }
    }
    if timeouts.connect > timeouts.first_byte {
        return Err(ConfigValidationError::InvalidTimeouts {
            message: format!(
                "upstream timeout \"connect\" ({:?}) must not exceed \"first_byte\" ({:?})",
                timeouts.connect, timeouts.first_byte
            ),
        });
    }
    if let Some(total) = timeouts.total
        && total < timeouts.first_byte
    {
        return Err(ConfigValidationError::InvalidTimeouts {
            message: format!(
                "upstream timeout \"total\" ({total:?}) must cover \"first_byte\" ({:?})",
                timeouts.first_byte
            ),
        });
    }
    Ok(())
}

/// Bare `/v1/*` routing (plan 04 step 11): off by default. When enabled, a
/// model name resolves to a provider or pool through `models`, falling back
/// to `default_target`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BareV1Config {
    /// Serve the bare `/v1/messages` and `/v1/chat/completions` routes.
    /// Default `false`: those routes 404.
    #[serde(default)]
    pub enabled: bool,
    /// Model name to provider or pool name.
    #[serde(default)]
    pub models: HashMap<String, String>,
    /// Target used when no `models` entry matches. `None` makes an unmapped
    /// model a 404 naming the model.
    #[serde(default)]
    pub default_target: Option<String>,
}

/// The retry/failover ladder for one route (plan 04 steps 2-3, 6). Present
/// only by opt-in on a provider (`[provider.retry]`) or a pool
/// (`[pool.retry]`); absent means exactly one attempt and no failover, the
/// pre-plan behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Attempts against one member before moving to the next (pools) or
    /// giving up (single provider). `1` disables same-member retries.
    pub max_attempts: u32,
    /// Base delay between attempts against the same member. An upstream
    /// `Retry-After` longer than this wins, clamped to the remaining
    /// deadline.
    #[serde(with = "humantime_serde")]
    pub backoff: Duration,
    /// How long a member that failed transiently is skipped by later
    /// requests. A 429 with real reset information overrides this until the
    /// reported reset time.
    #[serde(with = "humantime_serde")]
    pub cooldown: Duration,
    /// One deadline across every attempt of one request. `0s` disables it.
    #[serde(with = "humantime_serde")]
    pub total_timeout: Duration,
    /// Streaming pre-commit hold budget (plan 04 step 6): how long the route
    /// buffers waiting for the first output event before committing HTTP 200
    /// (after which heartbeats cover silence and errors are in-band).
    /// `0s` (the default) never commits without an event, keeping HTTP error
    /// semantics for as long as nothing was sent.
    #[serde(with = "humantime_serde")]
    pub hold: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            backoff: Duration::from_millis(250),
            cooldown: Duration::from_secs(30),
            total_timeout: Duration::ZERO,
            hold: Duration::ZERO,
        }
    }
}

/// What a pool does when a sticky session's pinned member must be switched
/// away from (plan 04 step 10, conflict 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AffinityConflictPolicy {
    /// Strip provider-bound request state (`provider_hints`) before
    /// switching. The default: availability over fidelity.
    #[default]
    Strip,
    /// Refuse the switch: the request fails rather than reaching a different
    /// account with state it cannot honour.
    Refuse,
}

/// Session affinity for a pool (plan 04 step 10): record session ids on usage
/// rows always; pinning a session to its last successful member is opt-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffinityConfig {
    /// Pin a session id to the member that last served it, so conversation
    /// cache warmth survives across requests. Default `false`.
    #[serde(default)]
    pub sticky: bool,
    /// What happens when the pinned member has to be switched away from.
    #[serde(default)]
    pub on_conflict: AffinityConflictPolicy,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            sticky: false,
            on_conflict: AffinityConflictPolicy::Strip,
        }
    }
}

/// Pool health settings (plan 04 step 9): passive cooldowns from 429 and
/// rate-limit headers are always on; the synthetic probe is opt-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolHealthConfig {
    /// How often to probe cooled-down members with a synthetic one-token
    /// request; a healthy answer clears the cooldown early. `None` (the
    /// default) disables probing — cooldowns then simply expire.
    #[serde(default, with = "humantime_serde::option")]
    pub probe_interval: Option<Duration>,
}

/// A named pool of providers served at `/providers/{pool}/v1/...`
/// (plan 04 step 4, conflict 1): fallback inside the pool, same URL shape as
/// a single provider. A pool name may not collide with a provider name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    /// Pool name; occupies the same URL namespace as provider names.
    pub name: String,
    /// Member provider names, in failover order. Duplicates and unknown
    /// names are rejected at config validation.
    pub members: Vec<String>,
    /// Retry ladder applied to the pool and inherited as the per-member
    /// attempt policy.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Session affinity policy for this pool.
    #[serde(default)]
    pub affinity: AffinityConfig,
    /// Health tracking for this pool's members.
    #[serde(default)]
    pub health: PoolHealthConfig,
}

/// Wrapper for a single pool TOML file (a `[pool]` table instead of a
/// `[provider]` one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolFile {
    /// The pool definition.
    pub pool: PoolConfig,
}

/// Default body limit for one request: 32 MiB (as before the family split).
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// The body limit a family applies, given the server defaults.
#[must_use]
pub fn family_max_body_bytes(server: &ServerConfig, family: &str) -> usize {
    server
        .endpoint_families
        .get(family)
        .map_or(DEFAULT_MAX_BODY_BYTES, |family_cfg| {
            family_cfg.max_body_bytes
        })
}

/// The request timeout a family applies, given the server default.
#[must_use]
pub fn family_request_timeout(server: &ServerConfig, family: &str) -> Duration {
    server
        .endpoint_families
        .get(family)
        .map_or(server.request_timeout, |cfg| cfg.request_timeout)
}

impl ServerConfig {
    /// Whether the dedicated admin listener binds at all.
    ///
    /// An explicit `admin_bind` always binds one. With none set, the
    /// default loopback listener binds unless `admin_on_main_listener`
    /// took over.
    #[must_use]
    pub fn dedicated_admin_listener(&self) -> bool {
        self.admin_bind.is_some() || !self.admin_on_main_listener
    }

    /// Bind address for the dedicated admin listener.
    ///
    /// `admin_bind` when configured; otherwise the default: `127.0.0.1` on
    /// the port after `bind`'s port. `None` when that default port cannot be
    /// derived (`bind` on port 65535) — `load_app_config` rejects that
    /// combination whenever the dedicated listener would bind the default.
    #[must_use]
    pub fn effective_admin_bind(&self) -> Option<SocketAddr> {
        match self.admin_bind {
            Some(addr) => Some(addr),
            None => self
                .bind
                .port()
                .checked_add(1)
                .map(|port| SocketAddr::new(std::net::IpAddr::from([127, 0, 0, 1]), port)),
        }
    }
}

const fn default_rate_limit_rpm() -> u32 {
    100
}

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
    /// in `Debug` output. `skip_serializing` keeps the hashes out of every
    /// `Serialize` path, so serializing then deserializing a config fails on
    /// this field — by design.
    #[serde(default, skip_serializing)]
    pub clients: HashMap<String, SecretString>,
    /// Admin API authentication (`[auth.admin]` section). When present, the
    /// read-only `/usage` query surface is enabled and every `/usage` route
    /// on every mount requires its bearer token.
    #[serde(default)]
    pub admin: Option<AdminAuthConfig>,
}

/// Admin API configuration (`[auth.admin]` section of `config.toml`).
///
/// A single bearer token guarding every `/usage` route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminAuthConfig {
    /// The admin bearer token, presented as `Authorization: Bearer <token>`.
    ///
    /// Compared in constant time against a SHA-256 digest of the configured
    /// token, so the plaintext never needs to be retained in memory. Wrapped
    /// in [`SecretString`] and `skip_serializing`, keeping it out of `Debug`
    /// output and every `Serialize` path.
    #[serde(skip_serializing)]
    pub token: SecretString,
}

const fn default_shutdown_timeout() -> Duration {
    Duration::from_secs(30)
}

const fn default_dedup_window() -> Duration {
    Duration::ZERO
}

const fn default_prune_interval() -> Duration {
    Duration::from_secs(3600)
}

/// Usage-ledger persistence configuration (`[storage]` section of
/// `config.toml`).
///
/// When present, every proxied request is persisted to a SQLite ledger by the
/// storage crate's event bus. When absent, the in-memory no-op bus is used and
/// nothing is written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Path to the SQLite database file. Parent directories are created on
    /// startup.
    pub path: PathBuf,
    /// Delete ledger rows older than this age. Absent means rows are kept
    /// forever. Must be positive when present.
    #[serde(default, with = "humantime_serde")]
    pub max_age: Option<Duration>,
    /// Keep at most this many ledger rows; the oldest rows are deleted when
    /// the cap is exceeded. Absent means unlimited. Must be at least 1 when
    /// present.
    #[serde(default)]
    pub max_rows: Option<u64>,
    /// How often the retention pruning pass runs. Defaults to 1 hour. Must be
    /// at least 1 second (the pruning timer rejects a zero period).
    #[serde(default = "default_prune_interval", with = "humantime_serde")]
    pub prune_interval: Duration,
}

fn validate_storage_config(storage: &StorageConfig) -> Result<(), CoreError> {
    if storage.path.as_os_str().is_empty() {
        return Err(CoreError::ConfigValidation {
            message: "storage.path must not be empty".to_owned(),
            source: None,
        });
    }
    if storage.max_age == Some(Duration::ZERO) {
        return Err(CoreError::ConfigValidation {
            message: "storage.max_age must be a positive duration; a zero age would \
                      delete every ledger row on every pruning pass"
                .to_owned(),
            source: None,
        });
    }
    if storage.max_rows == Some(0) {
        return Err(CoreError::ConfigValidation {
            message: "storage.max_rows must be at least 1; a cap of zero would keep \
                      no rows at all"
                .to_owned(),
            source: None,
        });
    }
    const MIN_PRUNE_INTERVAL: Duration = Duration::from_secs(1);
    if storage.prune_interval < MIN_PRUNE_INTERVAL {
        return Err(CoreError::ConfigValidation {
            message: format!(
                "storage.prune_interval must be at least 1 second, but is {:?}",
                storage.prune_interval
            ),
            source: None,
        });
    }
    Ok(())
}

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
    /// and zeroized on drop. Not serialized (`skip_serializing`): credentials
    /// never leak through serialization paths, so round-tripping a
    /// `ProviderFile` through serialize/deserialize FAILS because `api_key`
    /// is a required field. This is intentional.
    #[serde(skip_serializing)]
    pub api_key: SecretString,
    /// Additional upstream credentials (plan 04 step 5, backlog 31): when
    /// present and non-empty, these plus `api_key` form the provider's
    /// credential pool; the attempt loop rotates over them and cools a key
    /// down on auth and rate-limit failures. Never serialized.
    #[serde(default, skip_serializing)]
    pub api_keys: Option<Vec<SecretString>>,
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
    /// Opt-in retry ladder for this provider (plan 04 steps 2-3): absent
    /// means exactly one attempt, the pre-plan behaviour, and no failover on
    /// a direct `/providers/{provider}/...` route.
    #[serde(default)]
    pub retry: Option<RetryConfig>,
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
    /// Upstream egress (network exit) configuration. When present, this
    /// provider's upstream requests are sent through the configured forward
    /// proxy instead of a direct connection; absent means direct egress.
    #[serde(default)]
    pub egress: Option<ProviderEgressConfig>,
}

/// Per-provider forward-proxy egress configuration (`[provider.egress]`).
///
/// The proxy URL scheme must be one of `socks5h://`, `socks5://`, `http://`,
/// or `https://`; anything else is rejected at config load, never silently
/// ignored. `socks5h://` resolves upstream hostnames at the proxy (the exit
/// point's DNS view); `socks5://` resolves them locally.
///
/// The URL may carry userinfo (`http://user:pass@host:port`), which is passed
/// to the transport as proxy authentication. [`Debug`] redacts the userinfo;
/// only `scheme://host:port` is ever printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEgressConfig {
    /// Forward proxy URL (`socks5h://host:port`, `socks5://`, `http://`, or
    /// `https://`). Supports `${ENV_VAR}` interpolation like other string
    /// fields. The port is optional and defaults per scheme (1080 for socks).
    pub proxy: String,
}

impl std::fmt::Debug for ProviderEgressConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEgressConfig")
            .field("proxy", &redacted_proxy_url(&self.proxy))
            .finish()
    }
}

fn redacted_proxy_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<unparsed>".to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let after_authority = rest.strip_prefix(authority).unwrap_or_default();
    format!("{scheme}://{host}{after_authority}")
}

/// Provider-local model identifier used as a pricing key.
///
/// Transparent newtype over `String` so it serializes as a plain string in
/// TOML/JSON.
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

fn max_per_token_price() -> Decimal {
    Decimal::new(1_000, 0)
}

/// Protocol default allowlists for inbound client headers forwarded upstream
/// (row 12, backlog 10). Anthropic forwards its beta selector and session
/// headers by default so per-request beta features work through the proxy.
#[must_use]
pub fn default_forwarded_inbound_headers(protocol: &str) -> &'static [&'static str] {
    match protocol {
        "anthropic_messages" => &["anthropic-beta", "x-claude-code-session-id"],
        _ => &[],
    }
}

/// Inbound headers that are never forwarded upstream, even when configured:
/// client credentials, hop-by-hop and content headers the transport owns, and
/// browser headers that leak the caller's environment or break upstreams.
pub const NEVER_FORWARDED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    "set-cookie",
    "host",
    "connection",
    "content-length",
    "content-type",
    "accept",
    "accept-encoding",
    "accept-language",
    "origin",
    "referer",
    "user-agent",
    "x-request-id",
];

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

fn serialize_arc_headers<S>(
    headers: &Arc<HashMap<String, String>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    headers.as_ref().serialize(serializer)
}

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
    /// Wrapped in [`Arc`]: adapter headers are fixed at config-load time and
    /// shared across every request that resolves to this adapter, so downstream
    /// code that needs to mutate must materialize an owned copy.
    #[serde(
        default,
        serialize_with = "serialize_arc_headers",
        deserialize_with = "deserialize_arc_headers"
    )]
    pub headers: Arc<HashMap<String, String>>,
    /// Inbound client header names (case-insensitive) forwarded verbatim to
    /// this upstream. Unset uses the protocol default allowlist
    /// ([`default_forwarded_inbound_headers`]); headers on the
    /// never-forwarded list (client auth, `Origin`, browser and transport
    /// headers) are rejected at validation even when listed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_inbound_headers: Option<Vec<String>>,
    /// Per-route `lossy` opt-in: cross-format request fields with no provider
    /// equivalent are dropped with a logged warning instead of rejecting the
    /// request with a 400 that names them.
    #[serde(default)]
    pub lossy: bool,
}

impl ProviderAdapterConfig {
    fn forward_forwarded_for_debug(&self) -> &str {
        match &self.forward_inbound_headers {
            Some(_) => "<configured>",
            None => "<protocol default>",
        }
    }
}

impl std::fmt::Debug for ProviderAdapterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAdapterConfig")
            .field("protocol", &self.protocol)
            .field("endpoint", &endpoint_without_query(&self.endpoint))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field(
                "forward_inbound_headers",
                &self.forward_forwarded_for_debug(),
            )
            .field("lossy", &self.lossy)
            .finish()
    }
}

pub(crate) fn endpoint_without_query(endpoint: &str) -> &str {
    endpoint
        .split_once('?')
        .map_or(endpoint, |(base, _query)| base)
}

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

/// Supported discovery backends for fetching upstream model lists.
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
    Duration::from_secs(24 * 60 * 60)
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
    /// The `[provider.egress]` proxy URL is not usable as a forward proxy.
    #[error(
        "provider \"{provider}\": [provider.egress] proxy URL is invalid ({reason}): \"{proxy}\""
    )]
    InvalidEgressProxy {
        /// Provider name.
        provider: String,
        /// The rejected proxy URL.
        proxy: String,
        /// What is wrong with it.
        reason: &'static str,
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
    /// A configured inbound-header forwarding name is on the never-forwarded
    /// list (client credentials, hop-by-hop, browser headers).
    #[error(
        "provider \"{provider}\": adapter \"{adapter}\" may not forward inbound header \"{header}\" (client credentials, hop-by-hop, and browser headers are never forwarded)"
    )]
    ForbiddenForwardedHeader {
        /// Name of the offending provider.
        provider: String,
        /// Name of the offending adapter.
        adapter: String,
        /// The header name that may not be forwarded.
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
    /// A phase-shaped timeout rule is broken (backlog 8, BUG-14).
    #[error("invalid upstream timeouts: {message}")]
    InvalidTimeouts {
        /// What is wrong.
        message: String,
    },
    /// A pool definition is invalid (plan 04 step 4).
    #[error("pool \"{pool}\": {message}")]
    InvalidPool {
        /// Pool name.
        pool: String,
        /// What is wrong.
        message: String,
    },
    /// A retry ladder on a provider or pool is invalid.
    #[error("retry config for \"{route}\": {message}")]
    InvalidRetry {
        /// Provider or pool name.
        route: String,
        /// What is wrong.
        message: String,
    },
}

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
///     api_keys: None,
///     retry: None,
///     egress: None,
///     adapters: HashMap::from([(
///         "chat".to_owned(),
///         ProviderAdapterConfig {
///             protocol: "openai_chat_completions".to_owned(),
///             endpoint: "https://example.com/v1/chat/completions".to_owned(),
///             headers: Arc::new(HashMap::new()),
///             forward_inbound_headers: None,
///             lossy: false,
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
///     api_keys: None,
///     retry: None,
///     egress: None,
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
    validate_egress(provider, name)?;

    let uses_gemini_model_template = provider.adapters.values().any(|adapter| {
        adapter.protocol == "gemini_generate_content" && adapter.endpoint.contains("{model}")
    });
    validate_catalog(provider, name, uses_gemini_model_template)?;
    validate_routes(provider, name)?;
    validate_model_aliases(provider, name, uses_gemini_model_template)?;

    Ok(())
}

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

fn validate_api_key(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    if provider.passthrough_auth {
        return Ok(());
    }
    // Every key in the pool reaches the upstream verbatim, so each gets the
    // same checks as the base key.
    let extras = provider.api_keys.iter().flatten();
    for key in std::iter::once(&provider.api_key).chain(extras) {
        if let Some(var) = find_unresolved_env_var(key.expose_secret()) {
            return Err(ConfigValidationError::UnresolvedEnvVar {
                provider: name.to_owned(),
                var,
            });
        }
        if key.expose_secret().trim().is_empty() {
            return Err(ConfigValidationError::EmptyApiKey {
                provider: name.to_owned(),
            });
        }
    }
    if let Some(retry) = &provider.retry
        && retry.max_attempts == 0
    {
        return Err(ConfigValidationError::InvalidRetry {
            route: name.to_owned(),
            message: "retry.max_attempts must be at least 1".to_owned(),
        });
    }
    Ok(())
}

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
        validate_forwarded_inbound_headers(adapter_cfg, name, adapter_name)?;
    }
    Ok(())
}

/// Validates a configured inbound-header forwarding allowlist: names must be
/// non-empty, CRLF-free, and not on the never-forwarded list.
fn validate_forwarded_inbound_headers(
    adapter_cfg: &ProviderAdapterConfig,
    provider: &str,
    adapter: &str,
) -> Result<(), ConfigValidationError> {
    let Some(names) = &adapter_cfg.forward_inbound_headers else {
        return Ok(());
    };
    for header in names {
        if header.trim().is_empty() {
            return Err(ConfigValidationError::EmptyHeaderName {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
            });
        }
        if header.contains('\r') || header.contains('\n') {
            return Err(ConfigValidationError::HeaderContainsCrlf {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
            });
        }
        let lower = header.to_ascii_lowercase();
        if NEVER_FORWARDED_HEADERS.contains(&lower.as_str())
            || lower.starts_with("sec-")
            || lower.starts_with("access-control-")
        {
            return Err(ConfigValidationError::ForbiddenForwardedHeader {
                provider: provider.to_owned(),
                adapter: adapter.to_owned(),
                header: header.clone(),
            });
        }
    }
    Ok(())
}

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

fn has_valid_http_scheme(endpoint: &str) -> bool {
    let scheme_region = scheme_region(endpoint);
    scheme_region.eq_ignore_ascii_case("http") || scheme_region.eq_ignore_ascii_case("https")
}

fn scheme_region(endpoint: &str) -> &str {
    match endpoint.find("://") {
        Some(idx) => &endpoint[..idx],
        None => "",
    }
}

const EGRESS_PROXY_SCHEMES: [&str; 4] = ["socks5h", "socks5", "http", "https"];

fn validate_egress(provider: &ProviderConfig, name: &str) -> Result<(), ConfigValidationError> {
    let Some(egress) = &provider.egress else {
        return Ok(());
    };
    let url = egress.proxy.trim();
    if url.is_empty() {
        return Err(ConfigValidationError::InvalidEgressProxy {
            provider: name.to_owned(),
            proxy: egress.proxy.clone(),
            reason: "empty proxy URL",
        });
    }
    let scheme = scheme_region(url);
    if scheme.is_empty() {
        return Err(ConfigValidationError::InvalidEgressProxy {
            provider: name.to_owned(),
            proxy: egress.proxy.clone(),
            reason: "no URL scheme; expected socks5h://, socks5://, http://, or https://",
        });
    }
    if !EGRESS_PROXY_SCHEMES
        .iter()
        .any(|accepted| scheme.eq_ignore_ascii_case(accepted))
    {
        return Err(ConfigValidationError::InvalidEgressProxy {
            provider: name.to_owned(),
            proxy: egress.proxy.clone(),
            reason: "unsupported scheme; expected socks5h://, socks5://, http://, or https://",
        });
    }
    let authority = url
        .strip_prefix(scheme)
        .and_then(|after_scheme| after_scheme.strip_prefix("://"))
        .unwrap_or_default();
    let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.trim().is_empty() {
        return Err(ConfigValidationError::InvalidEgressProxy {
            provider: name.to_owned(),
            proxy: egress.proxy.clone(),
            reason: "no proxy host",
        });
    }
    Ok(())
}

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
    if let Some(storage) = cfg.storage.as_ref() {
        validate_storage_config(storage)?;
    }
    validate_admin_mount(&cfg)?;
    validate_upstream_timeouts(&cfg.server.upstream_timeouts).map_err(|error| {
        CoreError::ConfigValidation {
            message: error.to_string(),
            source: None,
        }
    })?;
    validate_bare_v1(&cfg)?;
    Ok(cfg)
}

/// Bare `/v1/*` validation within the app config alone: the model map must
/// not be empty while enabled without a default target (it could never
/// resolve anything). Targets are cross-checked against the loaded registry
/// where it is available — startup and `llm-proxy validate` — via
/// [`bare_v1_target_problems`], because provider files load after this runs;
/// an unresolved target still 404s at request time naming the target.
fn validate_bare_v1(cfg: &AppConfig) -> Result<(), CoreError> {
    let bare = &cfg.server.bare_v1;
    if !bare.enabled {
        return Ok(());
    }
    if bare.models.is_empty() && bare.default_target.is_none() {
        return Err(CoreError::ConfigValidation {
            message: "server.bare_v1 is enabled but has no model mappings and no \
                      default_target; no request could resolve"
                .to_owned(),
            source: None,
        });
    }
    Ok(())
}

/// Cross-checks every configured bare `/v1/*` target against `route_exists`
/// (the registry's provider and pool names). Returns one message per target
/// that names nothing loadable; a disabled [`BareV1Config`] has no targets to
/// check. Called by the startup path and the validate command, which see both
/// the app config and the registry.
#[must_use]
pub fn bare_v1_target_problems(
    bare: &BareV1Config,
    route_exists: impl Fn(&str) -> bool,
) -> Vec<String> {
    if !bare.enabled {
        return Vec::new();
    }
    let mut problems = Vec::new();
    for (model, target) in &bare.models {
        if !route_exists(target) {
            problems.push(format!(
                "server.bare_v1 model \"{model}\" targets unknown provider or pool \"{target}\""
            ));
        }
    }
    if let Some(target) = &bare.default_target
        && !route_exists(target)
    {
        problems.push(format!(
            "server.bare_v1 default_target \"{target}\" names no configured provider or pool"
        ));
    }
    problems
}

/// Validates one pool definition in isolation: non-empty name and members,
/// no duplicate members, a valid retry ladder. Membership and name collisions
/// with providers are checked by [`validate_registry_pools`], which sees both
/// sets.
///
/// # Errors
///
/// [`ConfigValidationError::InvalidPool`] naming the first broken rule.
pub fn validate_pool_config(pool: &PoolConfig) -> Result<(), ConfigValidationError> {
    validate_pool_config_against(pool, &[])
}

/// Validates a pool against the set of known provider names (which may be
/// empty when only structural checks are wanted).
pub(crate) fn validate_pool_config_against(
    pool: &PoolConfig,
    known_providers: &[String],
) -> Result<(), ConfigValidationError> {
    let invalid = |message: String| ConfigValidationError::InvalidPool {
        pool: pool.name.clone(),
        message,
    };
    if pool.name.trim().is_empty() {
        return Err(ConfigValidationError::EmptyProviderName);
    }
    if !pool_name_is_slug(&pool.name) {
        return Err(ConfigValidationError::InvalidProviderNameFormat {
            provider: pool.name.clone(),
        });
    }
    if pool.members.is_empty() {
        return Err(invalid(
            "pool has no members; add at least one provider name".to_owned(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for member in &pool.members {
        if member.trim().is_empty() {
            return Err(invalid(
                "member name is empty; members must name configured providers".to_owned(),
            ));
        }
        if !seen.insert(member.as_str()) {
            return Err(invalid(format!(
                "member \"{member}\" is listed more than once"
            )));
        }
        if !known_providers.is_empty() && !known_providers.iter().any(|p| p == member) {
            return Err(invalid(format!(
                "member \"{member}\" is not a configured provider"
            )));
        }
    }
    let retry = &pool.retry;
    if retry.max_attempts == 0 {
        return Err(invalid("retry.max_attempts must be at least 1".to_owned()));
    }
    Ok(())
}

/// A pool name is valid in the provider URL namespace under the same slug
/// rule as provider names.
fn pool_name_is_slug(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn validate_admin_mount(cfg: &AppConfig) -> Result<(), CoreError> {
    let Some(admin) = cfg.auth.admin.as_ref() else {
        if cfg.server.admin_bind.is_some() {
            return Err(CoreError::ConfigValidation {
                message: "server.admin_bind is configured but [auth.admin] is not; \
                          set [auth.admin] token or remove server.admin_bind"
                    .to_owned(),
                source: None,
            });
        }
        if cfg.server.admin_on_main_listener {
            return Err(CoreError::ConfigValidation {
                message: "server.admin_on_main_listener = true but [auth.admin] is not \
                          configured; there would be no admin token to guard /usage"
                    .to_owned(),
                source: None,
            });
        }
        return Ok(());
    };
    if admin.token.expose_secret().is_empty() {
        return Err(CoreError::ConfigValidation {
            message: "[auth.admin] token must not be empty".to_owned(),
            source: None,
        });
    }
    if cfg.storage.is_none() {
        return Err(CoreError::ConfigValidation {
            message: "[auth.admin] requires the [storage] usage ledger; configure \
                      [storage] or remove [auth.admin]"
                .to_owned(),
            source: None,
        });
    }
    if let Some(admin_bind) = cfg.server.admin_bind {
        if admin_bind == cfg.server.bind {
            return Err(CoreError::ConfigValidation {
                message: format!(
                    "server.admin_bind {admin_bind} must differ from server.bind; \
                     the admin listener needs its own socket"
                ),
                source: None,
            });
        }
    } else if !cfg.server.admin_on_main_listener && cfg.server.bind.port() == u16::MAX {
        return Err(CoreError::ConfigValidation {
            message: "server.bind port 65535 leaves no distinct port for the default \
                      admin listener; set server.admin_bind explicitly or enable \
                      server.admin_on_main_listener"
                .to_owned(),
            source: None,
        });
    }
    Ok(())
}

/// Reserved caller name used for unauthenticated requests under
/// [`AuthMode::None`]; a client key id must not collide with it or event
/// attribution becomes ambiguous.
pub const ANONYMOUS_CALLER: &str = "anonymous";

/// Refuses `passthrough_auth` providers combined with client-key auth.
///
/// With client keys on, the inbound token that `passthrough_auth` forwards
/// upstream is the proxy's own client key — the very secret that
/// authenticated the caller — so the provider would receive it. A token that
/// authenticated the caller must never be sent upstream; until a separate
/// upstream-key header exists, this combination is a configuration error.
pub fn validate_passthrough_auth_combination(
    auth: &AuthConfig,
    providers: &[ProviderConfig],
) -> Result<(), CoreError> {
    if auth.mode != AuthMode::ClientKeys {
        return Ok(());
    }
    let passthrough: Vec<&str> = providers
        .iter()
        .filter(|p| p.passthrough_auth)
        .map(|p| p.name.as_str())
        .collect();
    if passthrough.is_empty() {
        return Ok(());
    }
    Err(CoreError::ConfigValidation {
        message: format!(
            "auth.mode = \"client-keys\" cannot be combined with \
             passthrough_auth providers ({}): the forwarded inbound token would \
             be the proxy's own client key, leaking it to the provider. Remove \
             [auth.clients] / set auth.mode = \"none\", or configure an api_key \
             for these providers",
            passthrough.join(", ")
        ),
        source: None,
    })
}

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

    for (var_name, resolved) in find_env_var_refs(&raw) {
        match resolved {
            None => {}
            Some(value) if value.is_empty() => {
                return Err(ConfigValidationError::EmptyEnvVar {
                    provider: format!("(file: {})", path.display()),
                    var: var_name,
                }
                .into());
            }
            Some(_) => {}
        }
    }

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
    validate_provider_config(&file.provider, known_protocols).map_err(CoreError::from)?;
    Ok(file.provider)
}

#[cfg(test)]
mod tests {

    #[test]
    fn passthrough_providers_with_client_keys_are_refused() {
        let auth = AuthConfig {
            mode: AuthMode::ClientKeys,
            clients: HashMap::from([(
                "laptop".to_owned(),
                secrecy::SecretString::from("argon2id$hash"),
            )]),
            admin: None,
        };
        let mut provider = make_minimal_provider("passthrough-provider");
        provider.passthrough_auth = true;

        let err = validate_passthrough_auth_combination(&auth, &[provider])
            .expect_err("passthrough + client keys must be refused");
        let message = err.to_string();
        assert!(
            message.contains("passthrough-provider"),
            "the error must name the provider: {message}"
        );
        assert!(
            message.contains("client key"),
            "the error must say why: {message}"
        );
    }

    #[test]
    fn passthrough_providers_with_anonymous_auth_stay_allowed() {
        let auth = AuthConfig {
            mode: AuthMode::None,
            clients: HashMap::new(),
            admin: None,
        };
        let mut provider = make_minimal_provider("passthrough-provider");
        provider.passthrough_auth = true;

        validate_passthrough_auth_combination(&auth, &[provider])
            .expect("passthrough with anonymous auth is the intended use");
    }

    #[test]
    fn keyed_providers_with_client_keys_stay_allowed() {
        let auth = AuthConfig {
            mode: AuthMode::ClientKeys,
            clients: HashMap::from([(
                "laptop".to_owned(),
                secrecy::SecretString::from("argon2id$hash"),
            )]),
            admin: None,
        };
        let provider = make_minimal_provider("keyed-provider");

        validate_passthrough_auth_combination(&auth, &[provider])
            .expect("provider api keys with client keys are fine");
    }

    fn make_minimal_provider(name: &str) -> ProviderConfig {
        let mut provider = toml::from_str::<ProviderFile>(
            r#"[provider]
name = "placeholder"
api_key = "k"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#,
        )
        .expect("minimal provider parses")
        .provider;
        provider.name = name.to_owned();
        provider
    }

    #[test]
    fn upstream_timeouts_are_cross_validated() {
        validate_upstream_timeouts(&UpstreamTimeoutsConfig::default())
            .expect("defaults are internally consistent");

        let connect_gt_first_byte = UpstreamTimeoutsConfig {
            connect: Duration::from_secs(30),
            first_byte: Duration::from_secs(5),
            ..UpstreamTimeoutsConfig::default()
        };
        let err = validate_upstream_timeouts(&connect_gt_first_byte).expect_err("connect > first");
        assert!(
            err.to_string().contains("connect"),
            "the error names the offending phase: {err}"
        );

        let total_below_first_byte = UpstreamTimeoutsConfig {
            total: Some(Duration::from_secs(10)),
            first_byte: Duration::from_secs(60),
            ..UpstreamTimeoutsConfig::default()
        };
        let err = validate_upstream_timeouts(&total_below_first_byte).expect_err("total < first");
        assert!(
            err.to_string().contains("total"),
            "the error names the total: {err}"
        );

        let zero_idle = UpstreamTimeoutsConfig {
            idle: Duration::ZERO,
            ..UpstreamTimeoutsConfig::default()
        };
        assert!(validate_upstream_timeouts(&zero_idle).is_err());
    }

    #[test]
    fn upstream_timeouts_parse_from_toml_and_validate_at_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"

[server.upstream_timeouts]
connect = "5s"
first_byte = "15s"
idle = "30s"
total = "120s"
"#,
        )
        .expect("write config");

        let cfg = load_app_config(&cfg_path).expect("config loads");
        assert_eq!(cfg.server.upstream_timeouts.connect, Duration::from_secs(5));
        assert_eq!(
            cfg.server.upstream_timeouts.total,
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn invalid_upstream_timeouts_are_rejected_at_config_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"

[server.upstream_timeouts]
connect = "50s"
first_byte = "5s"
"#,
        )
        .expect("write config");

        let err = load_app_config(&cfg_path).expect_err("cross-validation fires at load");
        assert!(
            err.to_string().contains("first_byte"),
            "the load error names the broken phase rule: {err}"
        );
    }

    #[test]
    fn bare_v1_defaults_off_and_empty_enabled_config_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"

[server.bare_v1]
enabled = true
"#,
        )
        .expect("write config");

        let err = load_app_config(&cfg_path).expect_err("enabled bare_v1 needs a target");
        assert!(
            err.to_string().contains("bare_v1"),
            "the load error names bare_v1: {err}"
        );
    }

    #[test]
    fn bare_v1_targets_naming_nothing_loadable_are_reported() {
        let bare = BareV1Config {
            enabled: true,
            models: std::collections::HashMap::from([
                ("claude-sonnet-4-6".to_owned(), "real".to_owned()),
                ("gpt-4o".to_owned(), "ghost".to_owned()),
            ]),
            default_target: Some("also-ghost".to_owned()),
        };
        let problems = bare_v1_target_problems(&bare, |name| name == "real");
        assert_eq!(problems.len(), 2, "one problem per unknown target");
        assert!(
            problems[0].contains("gpt-4o") && problems[0].contains("ghost"),
            "the problem names the model and its target: {}",
            problems[0]
        );
        assert!(
            problems[1].contains("also-ghost"),
            "the default target is checked too: {}",
            problems[1]
        );
    }

    #[test]
    fn bare_v1_targets_that_name_loadable_routes_have_no_problems() {
        let bare = BareV1Config {
            enabled: true,
            models: std::collections::HashMap::from([("m".to_owned(), "pool-a".to_owned())]),
            default_target: Some("provider-b".to_owned()),
        };
        assert!(
            bare_v1_target_problems(&bare, |name| { name == "pool-a" || name == "provider-b" })
                .is_empty()
        );
    }

    #[test]
    fn disabled_bare_v1_targets_are_not_checked() {
        // Off by default: even targets that name nothing are not problems,
        // because the routes they would serve are not mounted.
        let bare = BareV1Config {
            enabled: false,
            models: std::collections::HashMap::from([("m".to_owned(), "ghost".to_owned())]),
            default_target: None,
        };
        assert!(bare_v1_target_problems(&bare, |_| false).is_empty());
    }

    #[test]
    fn provider_retry_and_api_keys_parse_from_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider_path = dir.path().join("p.toml");
        std::fs::write(
            &provider_path,
            r#"[provider]
name = "retried"
api_key = "k1"
api_keys = ["k2", "k3"]
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"

[provider.retry]
max_attempts = 3
backoff = "100ms"
cooldown = "45s"
total_timeout = "30s"
hold = "1s"
"#,
        )
        .expect("write provider");

        let provider = load_provider_config(&provider_path, None).expect("provider parses");
        let retry = provider.retry.expect("retry present");
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.backoff, Duration::from_millis(100));
        assert_eq!(retry.total_timeout, Duration::from_secs(30));
        assert_eq!(retry.hold, Duration::from_secs(1));
        assert_eq!(provider.api_keys.expect("key pool present").len(), 2);
    }

    use super::*;

    #[test]
    fn forwarded_inbound_allowlist_rejects_client_auth_headers() {
        let mut provider = make_minimal_provider("fwd-provider");
        let adapter = provider.adapters.get_mut("chat").expect("chat adapter");
        adapter.forward_inbound_headers = Some(vec![
            "anthropic-beta".to_owned(),
            "authorization".to_owned(),
        ]);
        let err = validate_provider_config(&provider, None)
            .expect_err("authorization must never be forwarded");
        assert!(
            matches!(&err, ConfigValidationError::ForbiddenForwardedHeader { header, .. } if header == "authorization"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn forwarded_inbound_allowlist_rejects_browser_and_origin_headers() {
        for header in [
            "origin",
            "user-agent",
            "cookie",
            "sec-fetch-mode",
            "x-request-id",
        ] {
            let mut provider = make_minimal_provider("fwd-provider");
            let adapter = provider.adapters.get_mut("chat").expect("chat adapter");
            adapter.forward_inbound_headers = Some(vec![header.to_owned()]);
            let err = validate_provider_config(&provider, None)
                .expect_err("browser/origin headers must never be forwarded");
            assert!(
                matches!(err, ConfigValidationError::ForbiddenForwardedHeader { .. }),
                "header {header} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn forwarded_inbound_allowlist_accepts_beta_and_session_headers() {
        let mut provider = make_minimal_provider("fwd-provider");
        let adapter = provider.adapters.get_mut("chat").expect("chat adapter");
        adapter.forward_inbound_headers = Some(vec![
            "anthropic-beta".to_owned(),
            "x-claude-code-session-id".to_owned(),
        ]);
        validate_provider_config(&provider, None)
            .expect("beta and session headers are forwardable");
    }

    #[test]
    fn anthropic_default_allowlist_carries_beta_and_session_headers() {
        assert_eq!(
            default_forwarded_inbound_headers("anthropic_messages"),
            &["anthropic-beta", "x-claude-code-session-id"]
        );
        assert!(default_forwarded_inbound_headers("openai_chat_completions").is_empty());
        assert!(default_forwarded_inbound_headers("gemini_generate_content").is_empty());
        assert!(default_forwarded_inbound_headers("openai_responses").is_empty());
    }

    #[test]
    fn lossy_flag_parses_from_toml() {
        let file = toml::from_str::<ProviderFile>(
            r#"[provider]
name = "lossy-provider"
api_key = "k"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"
lossy = true
forward_inbound_headers = ["x-app"]

[provider.routes]
chat_completions = "chat"
"#,
        )
        .expect("lossy provider parses");
        let adapter = file.provider.adapters.get("chat").expect("chat adapter");
        assert!(adapter.lossy);
        assert_eq!(
            adapter.forward_inbound_headers.as_deref(),
            Some(&["x-app".to_owned()][..])
        );
    }

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
        assert_eq!(config.server.shutdown_timeout, Duration::from_secs(30));
    }

    #[test]
    fn app_config_accepts_explicit_shutdown_timeout() {
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
        assert!(has_valid_http_scheme("http://host"));
        assert!(has_valid_http_scheme("https://host"));
        assert!(has_valid_http_scheme("HTTP://host"));
        assert!(has_valid_http_scheme("Https://host"));
        assert!(has_valid_http_scheme("HtTp://host/path"));
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
    fn egress_section_parses_and_absent_section_means_none() {
        let with_egress: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.egress]
proxy = "socks5h://127.0.0.1:1080"
"#,
        )
        .expect("provider with egress parses");
        assert_eq!(
            with_egress.provider.egress,
            Some(ProviderEgressConfig {
                proxy: "socks5h://127.0.0.1:1080".to_owned(),
            })
        );

        let without_egress: ProviderFile = toml::from_str(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"
"#,
        )
        .expect("provider without egress parses");
        assert_eq!(without_egress.provider.egress, None);
    }

    #[test]
    fn egress_section_rejects_unknown_fields() {
        let result = toml::from_str::<ProviderFile>(
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.egress]
proxy = "socks5h://127.0.0.1:1080"
bind_out = "198.51.100.10"
"#,
        );
        result.unwrap_err();
    }

    #[test]
    fn egress_debug_redacts_proxy_userinfo_but_keeps_the_host() {
        let egress = ProviderEgressConfig {
            proxy: "http://proxy-user:proxy-pass@proxy.internal:3128".to_owned(),
        };
        let debug = format!("{egress:?}");
        assert!(
            !debug.contains("proxy-user") && !debug.contains("proxy-pass"),
            "proxy credentials must never print: {debug}"
        );
        assert!(
            debug.contains("proxy.internal:3128"),
            "the credential-free host must stay visible for diagnostics: {debug}"
        );
        let raw = ProviderEgressConfig {
            proxy: "user:pass@host".to_owned(),
        };
        assert!(
            !format!("{raw:?}").contains("user:pass"),
            "unvalidated URLs must not be echoed: {:?}",
            raw
        );
    }

    #[test]
    fn validate_accepts_each_supported_egress_proxy_scheme() {
        for url in [
            "socks5h://tunnel.example.com:1080",
            "socks5://127.0.0.1:1080",
            "http://proxy.internal:3128",
            "https://proxy.internal:8443",
            "SOCKS5H://tunnel.example.com:1080",
            "http://user:pass@proxy.internal:3128",
            "socks5h://tunnel.example.com",
        ] {
            let file: ProviderFile = toml::from_str(&format!(
                r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.egress]
proxy = "{url}"
"#
            ))
            .expect("parses");
            assert_eq!(
                validate_provider_config(&file.provider, None),
                Ok(()),
                "egress proxy {url} must validate"
            );
        }
    }

    #[test]
    fn validate_rejects_unusable_egress_proxy_urls() {
        for (url, reason_fragment) in [
            ("ftp://proxy:21", "unsupported scheme"),
            ("socks4://127.0.0.1:1080", "unsupported scheme"),
            ("127.0.0.1:1080", "no URL scheme"),
            ("proxy.internal:3128", "no URL scheme"),
            ("socks5h://", "no proxy host"),
            ("http:///path", "no proxy host"),
            ("", "empty proxy URL"),
            ("   ", "empty proxy URL"),
        ] {
            let file: ProviderFile = toml::from_str(&format!(
                r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.egress]
proxy = "{url}"
"#
            ))
            .expect("parses");
            let err = validate_provider_config(&file.provider, None)
                .expect_err("egress proxy must be rejected");
            match &err {
                ConfigValidationError::InvalidEgressProxy {
                    provider, reason, ..
                } => {
                    assert_eq!(provider, "example");
                    assert!(
                        reason.contains(reason_fragment),
                        "reason {reason:?} must name {reason_fragment:?} for {url:?}"
                    );
                }
                other => panic!("expected InvalidEgressProxy for {url:?}, got {other:?}"),
            }
            let message = err.to_string();
            assert!(message.contains("example"));
            assert!(message.contains("[provider.egress]"));
        }
    }

    #[test]
    fn load_provider_config_rejects_invalid_egress_at_load_time() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("bad-egress.toml");
        std::fs::write(
            &path,
            r#"
[provider]
name = "example"
api_key = "key"
auth_style = "bearer"

[provider.egress]
proxy = "ftp://proxy.example.com:21"
"#,
        )
        .expect("write provider toml");

        let err = load_provider_config(&path, None).expect_err("load must reject bad egress");
        assert!(
            err.to_string().contains("egress"),
            "error must point at the egress section: {err}"
        );
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: String::new(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
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
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1".to_owned(),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
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
            name: "a".repeat(65),
            api_key: SecretString::from("key"),
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
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::InvalidProviderNameFormat { .. })
        ));
    }

    #[test]
    fn validate_accepts_provider_name_at_max_length() {
        let provider = ProviderConfig {
            name: "a".repeat(64),
            api_key: SecretString::from("key"),
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
        };
        validate_provider_config(&provider, None).unwrap();
    }

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
            api_keys: None,
            retry: None,
            egress: None,
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
            api_keys: None,
            retry: None,
            egress: None,
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
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
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
            pricing: HashMap::from([(
                ModelId::new("gpt-4o"),
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
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
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
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "JSON");
            assert_eq!(LogFormat::from_env(), LogFormat::Json);
        }
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "xml");
            assert_eq!(LogFormat::from_env(), LogFormat::Plain);
        }
        {
            let _g = crate::test_support::EnvVarGuard::set("RUST_LOG_FORMAT", "");
            assert_eq!(LogFormat::from_env(), LogFormat::Plain);
        }
    }

    #[test]
    fn validate_accepts_empty_api_key_when_passthrough_auth() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from(""),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: true,
            api_keys: None,
            retry: None,
            egress: None,
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
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from(""),
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
        };
        assert!(matches!(
            validate_provider_config(&provider, None),
            Err(ConfigValidationError::EmptyApiKey { .. })
        ));
    }

    #[test]
    fn validate_rejects_an_unresolved_env_var_in_the_extra_keys() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("base-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: Some(vec![
                SecretString::from("second-key"),
                SecretString::from("${LLM_PROXY_TEST_MISSING_KEY}"),
            ]),
            retry: None,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        match validate_provider_config(&provider, None) {
            Err(ConfigValidationError::UnresolvedEnvVar { provider, var }) => {
                assert_eq!(provider, "test");
                assert_eq!(var, "LLM_PROXY_TEST_MISSING_KEY");
            }
            other => panic!("an unresolved extra key must fail validation, got {other:?}"),
        }
    }

    fn write_app_config(extra_storage: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"
{extra_storage}"#
            ),
        )
        .unwrap();
        (directory, path)
    }

    #[test]
    fn storage_section_defaults_to_none_when_absent() {
        let (_dir, path) = write_app_config("");
        let config = load_app_config(&path).expect("valid config without [storage]");
        assert!(
            config.storage.is_none(),
            "absent [storage] must keep the no-op event bus"
        );
    }

    #[test]
    fn storage_section_parses_and_applies_defaults() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
"#,
        );
        let config = load_app_config(&path).expect("valid [storage]");
        let storage = config.storage.expect("storage present");
        assert_eq!(storage.path, std::path::PathBuf::from("/tmp/usage.db"));
        assert_eq!(storage.max_age, None, "absent max_age keeps rows forever");
        assert_eq!(storage.max_rows, None, "absent max_rows is unlimited");
        assert_eq!(
            storage.prune_interval,
            Duration::from_secs(3600),
            "absent prune_interval defaults to 1h"
        );
    }

    #[test]
    fn storage_section_parses_full_retention() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
max_age = "30d"
max_rows = 500_000
prune_interval = "15m"
"#,
        );
        let config = load_app_config(&path).expect("valid [storage]");
        let storage = config.storage.expect("storage present");
        assert_eq!(storage.max_age, Some(Duration::from_secs(30 * 24 * 3600)));
        assert_eq!(storage.max_rows, Some(500_000));
        assert_eq!(storage.prune_interval, Duration::from_secs(15 * 60));
    }

    #[test]
    fn storage_zero_prune_interval_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
prune_interval = "0s"
"#,
        );
        let error = load_app_config(&path).expect_err("zero prune interval rejected");
        assert!(
            error.to_string().contains("storage.prune_interval"),
            "got: {error}"
        );
    }

    #[test]
    fn storage_subsecond_prune_interval_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
max_age = "1d"
prune_interval = "500ms"
"#,
        );
        assert!(
            load_app_config(&path).is_err(),
            "sub-second prune interval must be rejected (timer panics on zero/near-zero)"
        );
    }

    #[test]
    fn storage_zero_max_age_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
max_age = "0s"
"#,
        );
        let error = load_app_config(&path).expect_err("zero max_age rejected");
        assert!(
            error.to_string().contains("storage.max_age"),
            "got: {error}"
        );
    }

    #[test]
    fn storage_zero_max_rows_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
max_rows = 0
"#,
        );
        let error = load_app_config(&path).expect_err("zero max_rows rejected");
        assert!(
            error.to_string().contains("storage.max_rows"),
            "got: {error}"
        );
    }

    #[test]
    fn storage_empty_path_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = ""
"#,
        );
        let error = load_app_config(&path).expect_err("empty path rejected");
        assert!(error.to_string().contains("storage.path"), "got: {error}");
    }

    #[test]
    fn storage_unknown_field_is_rejected() {
        let (_dir, path) = write_app_config(
            r#"
[storage]
path = "/tmp/usage.db"
db_host = "postgres.example"
"#,
        );
        assert!(
            load_app_config(&path).is_err(),
            "deny_unknown_fields must reject unrecognized [storage] keys"
        );
    }

    const STORAGE_SECTION: &str = "\n[storage]\npath = \"/tmp/usage.db\"\n";

    #[test]
    fn admin_section_parses_token_and_defaults_off() {
        let (_dir, path) = write_app_config(&format!(
            "{STORAGE_SECTION}\n[auth.admin]\ntoken = \"admin-secret\"\n"
        ));
        let config = load_app_config(&path).expect("valid [auth.admin]");
        assert!(
            config
                .auth
                .admin
                .as_ref()
                .is_some_and(|a| a.token.expose_secret() == "admin-secret"),
            "admin token must parse into the SecretString"
        );
        assert!(
            !config.server.admin_on_main_listener,
            "main-listener mount must be opt-in"
        );
        assert_eq!(
            config.server.effective_admin_bind(),
            Some("127.0.0.1:3457".parse().unwrap()),
            "default admin bind is loopback on the port after the main bind"
        );
        assert_eq!(
            config.admin_posture(),
            Some(AdminMountPosture::DedicatedListener),
            "the default posture is the dedicated listener"
        );
    }

    #[test]
    fn admin_explicit_bind_and_main_listener_opt_in_parse() {
        let (_dir, path) = write_app_config(&format!(
            "\nadmin_bind = \"127.0.0.1:9900\"\nadmin_on_main_listener = true\n{STORAGE_SECTION}\n\
             [auth.admin]\ntoken = \"admin-secret\"\n"
        ));
        let config = load_app_config(&path).expect("valid admin config");
        assert_eq!(
            config.server.effective_admin_bind(),
            Some("127.0.0.1:9900".parse().unwrap()),
            "explicit admin_bind wins over the derived default"
        );
        assert_eq!(
            config.admin_posture(),
            Some(AdminMountPosture::MainListener),
            "admin_on_main_listener selects the main-listener posture"
        );
        assert_eq!(
            AdminMountPosture::MainListener.name(),
            "admin-on-main-listener",
            "posture names are stable for log grepping"
        );
    }

    #[test]
    fn admin_without_storage_is_rejected() {
        let (_dir, path) = write_app_config("\n[auth.admin]\ntoken = \"admin-secret\"\n");
        let error = load_app_config(&path).expect_err("admin requires [storage]");
        assert!(
            error.to_string().contains("[auth.admin] requires"),
            "got: {error}"
        );
    }

    #[test]
    fn admin_empty_token_is_rejected() {
        let (_dir, path) =
            write_app_config(&format!("{STORAGE_SECTION}\n[auth.admin]\ntoken = \"\"\n"));
        let error = load_app_config(&path).expect_err("empty admin token rejected");
        assert!(
            error.to_string().contains("token must not be empty"),
            "got: {error}"
        );
    }

    #[test]
    fn admin_bind_equal_to_main_bind_is_rejected() {
        let (_dir, path) = write_app_config(&format!(
            "\nadmin_bind = \"127.0.0.1:3456\"\n{STORAGE_SECTION}\n[auth.admin]\n\
             token = \"admin-secret\"\n"
        ));
        let error = load_app_config(&path).expect_err("same-socket bind rejected");
        assert!(
            error.to_string().contains("must differ from server.bind"),
            "got: {error}"
        );
    }

    #[test]
    fn admin_default_port_underviable_on_65535_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\nbind = \"127.0.0.1:65535\"\nrequest_timeout = \"300s\"\n\
                 log_level = \"info\"\nhot_reload = false\nserver_name = \"t\"\n{STORAGE_SECTION}\n\
                 \n[auth.admin]\ntoken = \"admin-secret\"\n"
            ),
        )
        .unwrap();
        let error = load_app_config(&path).expect_err("no derivable admin port");
        assert!(error.to_string().contains("65535"), "got: {error}");
    }

    #[test]
    fn admin_main_listener_optin_needs_no_derivable_port() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\nbind = \"127.0.0.1:65535\"\nrequest_timeout = \"300s\"\n\
                 log_level = \"info\"\nhot_reload = false\nserver_name = \"t\"\n\
                 admin_on_main_listener = true\n{STORAGE_SECTION}\n\
                 \n[auth.admin]\ntoken = \"admin-secret\"\n"
            ),
        )
        .unwrap();
        let config = load_app_config(&path).expect("main-listener posture needs no port");
        assert!(
            !config.server.dedicated_admin_listener(),
            "the opt-in without an explicit admin_bind must not bind a dedicated socket"
        );
    }

    #[test]
    fn explicit_admin_bind_keeps_the_dedicated_listener_under_the_optin() {
        let (_dir, path) = write_app_config(&format!(
            "\nadmin_bind = \"127.0.0.1:9900\"\nadmin_on_main_listener = true\n{STORAGE_SECTION}\n\
             [auth.admin]\ntoken = \"admin-secret\"\n"
        ));
        let config = load_app_config(&path).expect("both mounts are legitimate");
        assert!(
            config.server.dedicated_admin_listener(),
            "an explicit admin_bind always binds the dedicated listener"
        );
        assert_eq!(
            config.admin_posture(),
            Some(AdminMountPosture::MainListener),
            "and the main-listener mount is on top of it"
        );
    }

    #[test]
    fn admin_bind_without_admin_section_is_rejected() {
        let (_dir, path) = write_app_config(&format!(
            "\nadmin_bind = \"127.0.0.1:9900\"\n{STORAGE_SECTION}"
        ));
        let error = load_app_config(&path).expect_err("dead admin_bind rejected");
        assert!(
            error
                .to_string()
                .contains("server.admin_bind is configured"),
            "got: {error}"
        );
    }

    #[test]
    fn admin_main_listener_flag_without_admin_section_is_rejected() {
        let (_dir, path) = write_app_config(&format!(
            "\nadmin_on_main_listener = true\n{STORAGE_SECTION}"
        ));
        let error = load_app_config(&path).expect_err("dead flag rejected");
        assert!(
            error.to_string().contains("admin_on_main_listener"),
            "got: {error}"
        );
    }

    #[test]
    fn admin_unknown_field_is_rejected() {
        let (_dir, path) = write_app_config(&format!(
            "{STORAGE_SECTION}\n[auth.admin]\ntoken = \"admin-secret\"\nrotates = \"weekly\"\n"
        ));
        assert!(
            load_app_config(&path).is_err(),
            "deny_unknown_fields must reject unrecognized [auth.admin] keys"
        );
    }

    #[test]
    fn admin_posture_is_none_without_admin_section() {
        let (_dir, path) = write_app_config(STORAGE_SECTION);
        let config = load_app_config(&path).expect("valid config");
        assert_eq!(
            config.admin_posture(),
            None,
            "no [auth.admin] means no admin surface anywhere"
        );
    }

    #[test]
    fn admin_auth_config_debug_redacts_token() {
        let admin = AdminAuthConfig {
            token: SecretString::from("super-secret-admin-token"),
        };
        let debug = format!("{admin:?}");
        assert!(
            !debug.contains("super-secret-admin-token"),
            "Debug must redact the admin token, got: {debug}"
        );
    }
}
