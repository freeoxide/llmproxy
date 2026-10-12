//! Middleware components for the LLM proxy server.
//!
//! Provides request deduplication, rate limiting, request ID generation,
//! client IP extraction, and client-key authentication state
//! ([`ClientAuth`], [`CallerId`]).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use llm_proxy_core::{ANONYMOUS_CALLER, AuthConfig, AuthMode};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};

use argon2::PasswordVerifier;

// ---------------------------------------------------------------------------
// RequestDeduplicator
// ---------------------------------------------------------------------------

/// Maximum number of in-flight deduplication entries. Prevents unbounded
/// memory growth from unique-body requests.
const MAX_DEDUP_ENTRIES: usize = 10_000;

/// Maximum body size (in bytes) eligible for dedup tracking. Bodies larger
/// than this are never hashed or tracked to avoid expensive SHA-256 computation
/// on large payloads and prevent memory pressure from storing many hash entries.
/// 64 KiB is chosen as a reasonable threshold: most LLM API requests with
/// moderate context fit within this limit, while very large file uploads or
/// multi-turn conversations with huge contexts are excluded from dedup.
const MAX_DEDUP_BODY_SIZE: usize = 64 * 1024;

/// Deduplicates requests based on a SHA-256 hash of the request path and body.
///
/// Tracks in-flight request hashes with a configurable deduplication window.
/// The standalone type defaults to 500 ms; server configuration defaults to
/// disabled. If a second request with the same path and body arrives during a
/// nonzero window, the duplicate is rejected.
///
/// The request path is included in the hash so that the same body sent to
/// different protocol endpoints (e.g. `/v1/messages` vs `/v1/chat/completions`)
/// is not treated as a duplicate.
///
/// # Why `std::sync::Mutex`?
///
/// Uses `std::sync::Mutex` rather than `tokio::sync::Mutex` because the
/// critical section is purely in-memory HashMap operations (insert, remove,
/// retain) that complete in microseconds. `tokio::sync::Mutex` would add
/// unnecessary async overhead for a non-I/O-bound lock. The mutex is never
/// held across `.await` points.
#[derive(Debug)]
pub struct RequestDeduplicator {
    /// SHA-256 raw digest -> insertion time. Keyed on the 32-byte digest
    /// (`[u8; 32]` is `Copy + Eq + Hash`) rather than a 64-char hex `String`
    /// to avoid a per-request allocation and formatting pass (audit LOW-8).
    in_flight: Mutex<HashMap<[u8; 32], Instant>>,
    /// Deduplication window in milliseconds.
    window_ms: u64,
}

impl RequestDeduplicator {
    /// Create a new deduplicator with a 500 ms window.
    pub fn new() -> Self {
        Self::with_window_ms(500)
    }

    /// Create a new deduplicator with a custom deduplication window.
    pub fn with_window_ms(window_ms: u64) -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
            window_ms,
        }
    }

    /// Check whether this request body is a duplicate.
    ///
    /// Returns `true` if the request is a duplicate (should be rejected),
    /// `false` if it is new (should be processed).
    ///
    /// # Deprecation
    ///
    /// Prefer [`Self::is_duplicate_with_path`] which includes the request path in the
    /// hash to distinguish requests to different protocol endpoints.
    #[deprecated(note = "use is_duplicate_with_path instead")]
    pub fn is_duplicate(&self, body: &[u8]) -> bool {
        self.is_duplicate_with_path("", body)
    }

    /// Check whether this request is a duplicate, keyed by path and body.
    ///
    /// Including the path distinguishes requests to different protocol
    /// endpoints (e.g. `/v1/messages` vs `/v1/chat/completions`) that
    /// happen to carry the same body.
    pub fn is_duplicate_with_path(&self, path: &str, body: &[u8]) -> bool {
        if self.window_ms == 0 {
            return false;
        }
        // Skip dedup for large bodies to avoid expensive hashing and memory pressure.
        if body.len() > MAX_DEDUP_BODY_SIZE {
            return false;
        }
        let hash = Self::hash_path_body(path, body);
        let now = Instant::now();

        let mut map = self.in_flight.lock().unwrap_or_else(|e| {
            tracing::warn!("request deduplicator mutex poisoned; recovering from panic");
            e.into_inner()
        });

        // Prune expired entries.
        map.retain(|_, t| now.duration_since(*t).as_millis() < self.window_ms as u128);

        if map.contains_key(&hash) {
            return true;
        }

        // Skip dedup tracking when at capacity to bound memory usage.
        if map.len() < MAX_DEDUP_ENTRIES {
            map.insert(hash, now);
        }
        false
    }

    /// Compute the SHA-256 digest of the request path and body.
    ///
    /// Returns the raw 32-byte digest directly (rather than a 64-char hex
    /// `String`) since `[u8; 32]` is `Copy + Eq + Hash` and serves as the
    /// `in_flight` key without any formatting pass or heap allocation
    /// (audit LOW-8).
    fn hash_path_body(path: &str, body: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(path.as_bytes());
        hasher.update(body);
        hasher.finalize().into()
    }
}

impl Default for RequestDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Maximum number of per-IP rate-limit buckets. Prevents unbounded memory
/// growth from spoofed or diverse source IPs.
const MAX_RATE_LIMIT_BUCKETS: usize = 100_000;

/// Per-client token bucket for rate limiting.
#[derive(Debug)]
struct ClientTokenBucket {
    /// Remaining tokens.
    tokens: f64,
    /// Maximum tokens.
    max_tokens: f64,
    /// Timestamp of last refill.
    last_refill: Instant,
}

impl ClientTokenBucket {
    fn new(max_tokens: f64) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns `true` if allowed.
    fn try_consume(&mut self, refill_rate: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-IP rate limiter using token buckets.
///
/// Each client IP gets its own bucket allowing `max_requests_per_minute`
/// requests per minute. Pruning of stale entries occurs periodically
/// (every `PRUNE_INTERVAL` requests) rather than on every call to reduce
/// per-request overhead under high load.
#[derive(Debug)]
pub struct RateLimiter {
    /// Per-IP token buckets.
    buckets: Mutex<HashMap<String, ClientTokenBucket>>,
    /// Maximum requests per minute per client.
    max_requests_per_minute: f64,
    /// Counter for periodic pruning. Pruning runs every `PRUNE_INTERVAL` requests.
    prune_counter: AtomicU64,
}

/// Number of requests between prune sweeps. Trade-off: lower values prune
/// more aggressively but add overhead; higher values reduce overhead but
/// allow stale entries to linger longer.
const PRUNE_INTERVAL: u64 = 128;

/// Idle eviction threshold for per-IP rate-limit buckets (5 minutes).
///
/// Buckets whose last refill was more than this duration ago are evicted
/// during periodic pruning to prevent unbounded memory growth.
const RATE_LIMITER_IDLE_EVICT_SECS: u64 = 300;

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// `max_requests_per_minute` is the maximum number of requests allowed
    /// per client IP per minute. `0` disables rate limiting.
    ///
    /// # Panics
    ///
    /// Does not panic, but extremely large values (e.g. `u32::MAX`) create
    /// per-IP buckets with billions of tokens. Operators should choose
    /// reasonable values (e.g. 100--100_000 RPM) to avoid pathological
    /// memory usage from unbounded per-IP bucket allocation.
    pub fn new(max_requests_per_minute: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            max_requests_per_minute: max_requests_per_minute as f64,
            prune_counter: AtomicU64::new(0),
        }
    }

    /// Check whether a request from `client_ip` is allowed.
    ///
    /// Returns `true` if the request is allowed, `false` if rate-limited.
    pub fn is_allowed(&self, client_ip: &str) -> bool {
        if self.max_requests_per_minute == 0.0 {
            return true;
        }
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| {
            tracing::warn!("rate limiter mutex poisoned; recovering from panic");
            e.into_inner()
        });

        // Periodic pruning: evict stale entries every PRUNE_INTERVAL requests
        // to prevent unbounded memory growth without paying the cost on every call.
        if self.prune_counter.fetch_add(1, Ordering::Relaxed) % PRUNE_INTERVAL == 0 {
            let now = std::time::Instant::now();
            buckets.retain(|_, bucket| {
                now.duration_since(bucket.last_refill)
                    < std::time::Duration::from_secs(RATE_LIMITER_IDLE_EVICT_SECS)
            });
        }

        // Refill rate: tokens per second.
        let refill_rate = self.max_requests_per_minute / 60.0;

        // Single-lookup hot path: for a known IP we mutate the existing bucket
        // in place without allocating a key `String`. Only on a miss do we
        // perform the capacity check and a second (entry) probe that allocates.
        // This avoids the redundant `contains_key` + `entry` double hash probe
        // and the unconditional `to_owned()` on every call (audit LOW-6).
        if let Some(bucket) = buckets.get_mut(client_ip) {
            return bucket.try_consume(refill_rate);
        }

        // New IP: reject if inserting it would exceed the bucket cap. Bounds
        // memory growth from spoofed or diverse source IPs.
        if buckets.len() >= MAX_RATE_LIMIT_BUCKETS {
            return false;
        }

        let bucket = buckets
            .entry(client_ip.to_owned())
            .or_insert_with(|| ClientTokenBucket::new(self.max_requests_per_minute));

        bucket.try_consume(refill_rate)
    }

    /// Whether a request from `client_ip` would currently be allowed,
    /// **without consuming a token**.
    ///
    /// Pure read of the peer's current refilled balance: a peer with no
    /// bucket yet would start full, so it reports `true`. Used by the
    /// client-auth gate to reject peers whose failure budget is already
    /// exhausted *before* running the CPU-costly argon2 verification; the
    /// actual token consumption still happens on the failure path via
    /// [`Self::is_allowed`], so successful authentications are never charged.
    #[must_use]
    pub fn would_allow(&self, client_ip: &str) -> bool {
        if self.max_requests_per_minute == 0.0 {
            return true;
        }
        let buckets = self.buckets.lock().unwrap_or_else(|e| {
            tracing::warn!("rate limiter mutex poisoned; recovering from panic");
            e.into_inner()
        });
        let Some(bucket) = buckets.get(client_ip) else {
            return true;
        };
        // Same refill math as `try_consume`, but without mutating the bucket:
        // compute the balance as of now and check it holds at least one token.
        let elapsed = Instant::now()
            .duration_since(bucket.last_refill)
            .as_secs_f64();
        let refill_rate = self.max_requests_per_minute / 60.0;
        let tokens = (bucket.tokens + elapsed * refill_rate).min(bucket.max_tokens);
        tokens >= 1.0
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(100)
    }
}

// ---------------------------------------------------------------------------
// RequestIdGenerator
// ---------------------------------------------------------------------------

/// Generates unique request IDs in the format `req-{unix}-{counter}`.
#[derive(Debug)]
pub struct RequestIdGenerator {
    counter: AtomicU64,
}

impl RequestIdGenerator {
    /// Create a new request ID generator.
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    /// Generate the next request ID.
    ///
    /// The ID format is `req-{unix_seconds}-{monotonic_counter}`.
    ///
    /// # Clock fallback
    ///
    /// If `SystemTime::now()` is before `UNIX_EPOCH` (nearly impossible on
    /// modern systems), the timestamp falls back to 0. The monotonic counter
    /// still guarantees uniqueness within the process.
    pub fn next_id(&self) -> String {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "system clock is before UNIX epoch; using 0 as request ID timestamp");
                0
            });
        // Ordering::Relaxed is sufficient: the counter only needs to be
        // monotonic within this process. No other memory depends on the
        // counter value for synchronization.
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("req-{unix}-{counter}")
    }
}

impl Default for RequestIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed request identifier carried in request extensions.
///
/// Inserted by an outermost `middleware::from_fn` layer so that the
/// `TraceLayer` span (`make_span_with`) and the route handler share a single
/// id. This keeps the id that appears in the `x-request-id` response header and
/// in handler log events identical to the id stamped on the request's tracing
/// span, restoring end-to-end span/event correlation (audit MEDIUM-1).
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Client IP extraction
// ---------------------------------------------------------------------------

/// Optional peer socket metadata for handlers that also run in router-only tests.
#[derive(Clone, Copy, Debug)]
pub struct OptionalConnectInfo(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for OptionalConnectInfo
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let address = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|connect_info| connect_info.0);
        async move { Ok(Self(address)) }
    }
}

/// Extract the client IP from trusted forwarding headers or connection info.
///
/// When `trust_forwarded_headers` is true, the leftmost value from
/// `X-Forwarded-For` or the value from `X-Real-Ip` is used, but only after
/// validating it parses as a legitimate [`std::net::IpAddr`]. Invalid or
/// spoofed values are silently ignored and the connection-info fallback is
/// used instead.
///
/// # Trust model
///
/// The **leftmost** `X-Forwarded-For` entry is trusted as the client address.
/// This is only correct behind a **single** reverse proxy that strips or
/// overwrites any inbound `X-Forwarded-For` before appending its own hop. With
/// multiple untrusted hops the leftmost entry is attacker-controllable (a
/// client may prepend arbitrary IPs), so the extracted address must not be
/// trusted for authentication, rate-limit bypass, or audit logging. The IpAddr
/// parse check defends against malformed garbage, not against well-formed
/// spoofed values.
pub fn get_client_ip(
    headers: &axum::http::HeaderMap,
    connect_info: Option<&SocketAddr>,
    trust_forwarded_headers: bool,
) -> String {
    if trust_forwarded_headers {
        // Check X-Forwarded-For first (leftmost IP).
        if let Some(xff) = headers.get("x-forwarded-for")
            && let Ok(val) = xff.to_str()
            && let Some(ip) = val.split(',').next()
        {
            let trimmed = ip.trim();
            if trimmed.parse::<std::net::IpAddr>().is_ok() {
                return trimmed.to_owned();
            }
        }

        // Check X-Real-Ip.
        if let Some(xri) = headers.get("x-real-ip")
            && let Ok(val) = xri.to_str()
        {
            let trimmed = val.trim();
            if trimmed.parse::<std::net::IpAddr>().is_ok() {
                return trimmed.to_owned();
            }
        }
    }

    // Fall back to connection info.
    if let Some(ci) = connect_info {
        return ci.ip().to_string();
    }

    "unknown".to_owned()
}

// ---------------------------------------------------------------------------
// Client-key authentication
// ---------------------------------------------------------------------------

/// One configured client key: its id and its PHC-format argon2 verifier.
///
/// The verifier is the *hashed* secret only; the plaintext client key exists
/// solely in the operator's records and the client's `Authorization` header,
/// never in this process's long-lived state.
struct ClientVerifier {
    /// Operator-chosen key id used for caller attribution and rate-limit keys.
    key_id: String,
    /// PHC-format argon2 hash of the client key (e.g.
    /// `$argon2id$v=19$m=19456,t=2,p=1$...`). Parsed (and therefore
    /// validated) whenever the verifier is checked; the raw string is kept
    /// because `PasswordHash` borrows its input.
    phc: SecretString,
}

/// SHA-256 digest of an authenticated client key, with the key id it matched.
struct CachedDigest {
    digest: [u8; 32],
    key_id: String,
}

/// Constant-time equality of two equal-length byte strings.
///
/// Compares all bytes unconditionally so the runtime does not depend on where
/// the first difference appears. Length mismatch returns `false` immediately;
/// digest lengths are fixed at 32 bytes here, so that branch is not secret
/// dependent.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Errors raised while building [`ClientAuth`] from configuration.
#[derive(Debug, thiserror::Error)]
pub enum ClientAuthError {
    /// `auth.mode = "none"` was combined with configured client keys.
    #[error("auth.mode is \"none\" but [auth.clients] is non-empty")]
    ModeNoneWithClients,
    /// `auth.mode = "client-keys"` had no client keys configured.
    #[error("auth.mode is \"client-keys\" but [auth.clients] is empty")]
    ClientKeysWithoutClients,
    /// A key id was empty or the reserved `anonymous` name.
    #[error("client key id \"{0}\" is empty or reserved")]
    InvalidKeyId(String),
    /// A verifier string is not a parseable PHC argon2 hash.
    ///
    /// The error carries the key id, never the hash string itself.
    #[error("client key \"{key_id}\" has an invalid PHC verifier: {source}")]
    InvalidVerifier {
        /// The key id whose verifier failed to parse.
        key_id: String,
        /// The underlying parse error.
        source: argon2::password_hash::Error,
    },
}

/// Client-key authentication state: the configured verifiers plus a digest
/// cache for verified keys.
///
/// Holds hashed verifiers only, never plaintext secrets. A presented bearer
/// token is verified with argon2 (constant-time tag comparison inside the
/// KDF); after a first successful verification the SHA-256 digest of the
/// token is cached and compared in constant time, so steady-state requests
/// skip the (CPU-heavy) KDF entirely. Only digests of tokens that already
/// verified are cached, so the cache holds at most one entry per configured
/// key id.
///
/// Manual `Debug`: prints the mode and the key ids, never the verifier
/// strings (a leaked PHC hash enables offline brute-force of the key).
pub struct ClientAuth {
    /// Whether proxy routes must present a valid client key. True whenever
    /// verifiers were configured (`auth.mode = "client-keys"`).
    enforced: bool,
    /// Configured verifiers, sorted by key id for deterministic iteration.
    verifiers: Vec<ClientVerifier>,
    /// Digest cache: SHA-256(token) -> key id for tokens that verified.
    cache: Mutex<Vec<CachedDigest>>,
}

impl std::fmt::Debug for ClientAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientAuth")
            .field("enforced", &self.enforced)
            .field(
                "client_key_ids",
                &self
                    .verifiers
                    .iter()
                    .map(|v| v.key_id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ClientAuth {
    /// Build authentication state from the `[auth]` config section.
    ///
    /// Validates mode consistency, key ids, and that every verifier parses as
    /// a PHC argon2 string. `auth.mode = "none"` builds a disabled
    /// (not enforced) authenticator; the caller is responsible for the
    /// loopback-bind rule that `load_app_config` enforces for that mode.
    ///
    /// # Errors
    ///
    /// Returns [`ClientAuthError`] on contradictory mode/client combinations,
    /// reserved or empty key ids, or an unparseable verifier string.
    pub fn from_config(config: &AuthConfig) -> Result<Self, ClientAuthError> {
        match config.mode {
            AuthMode::None if !config.clients.is_empty() => {
                return Err(ClientAuthError::ModeNoneWithClients);
            }
            AuthMode::ClientKeys if config.clients.is_empty() => {
                return Err(ClientAuthError::ClientKeysWithoutClients);
            }
            AuthMode::None | AuthMode::ClientKeys => {}
        }
        let mut verifiers = Vec::with_capacity(config.clients.len());
        for (key_id, phc) in &config.clients {
            if key_id.is_empty() || key_id == ANONYMOUS_CALLER {
                return Err(ClientAuthError::InvalidKeyId(key_id.clone()));
            }
            // Validate parseability now so a malformed verifier fails startup
            // (via `load_toml_state`) instead of failing every request later.
            if let Err(source) = argon2::password_hash::phc::PasswordHash::new(phc.expose_secret())
            {
                return Err(ClientAuthError::InvalidVerifier {
                    key_id: key_id.clone(),
                    source: source.into(),
                });
            }
            verifiers.push(ClientVerifier {
                key_id: key_id.clone(),
                phc: SecretString::from(phc.expose_secret().to_owned()),
            });
        }
        verifiers.sort_by(|a, b| a.key_id.cmp(&b.key_id));
        Ok(Self {
            enforced: matches!(config.mode, AuthMode::ClientKeys),
            verifiers,
            cache: Mutex::new(Vec::new()),
        })
    }

    /// Fail-closed authenticator: enforcement on, nothing can authenticate.
    ///
    /// Used when config is invalid at construction time so every proxied
    /// request is rejected rather than silently downgraded to no-auth.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            enforced: true,
            verifiers: Vec::new(),
            cache: Mutex::new(Vec::new()),
        }
    }

    /// Whether proxy routes require a valid client key.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.enforced
    }

    /// Number of configured client keys (0 when not enforced).
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.verifiers.len()
    }

    /// Look up a cached match for `digest` in constant time.
    fn cached_key_id(&self, digest: &[u8; 32]) -> Option<String> {
        let cache = self.cache.lock().unwrap_or_else(|e| {
            tracing::warn!("client auth digest cache poisoned; recovering from panic");
            e.into_inner()
        });
        cache
            .iter()
            .find(|entry| constant_time_eq(&entry.digest, digest))
            .map(|entry| entry.key_id.clone())
    }

    /// Verify a presented bearer token against every configured verifier.
    ///
    /// Runs the argon2 KDFs on the blocking pool (a single verification with
    /// default parameters costs tens of milliseconds; running it on the async
    /// executor would stall every other future on the worker). Returns the
    /// matching key id, or `None` for unknown/invalid tokens. The token is
    /// never logged.
    async fn verify_uncached(&self, token: &str) -> Option<String> {
        // Digest before the move into the blocking task; needed for the
        // cache insert when verification succeeds.
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let verifiers: Vec<(String, String)> = self
            .verifiers
            .iter()
            .map(|v| (v.key_id.clone(), v.phc.expose_secret().to_owned()))
            .collect();
        let token = token.to_owned();
        let matched = tokio::task::spawn_blocking(move || {
            let argon2 = argon2::Argon2::default();
            for (key_id, phc) in &verifiers {
                // `verify_password` derives the tag with the parameters
                // embedded in the PHC string and compares in constant time.
                if let Ok(hash) = argon2::password_hash::phc::PasswordHash::new(phc)
                    && argon2.verify_password(token.as_bytes(), &hash).is_ok()
                {
                    return Some(key_id.clone());
                }
            }
            None
        })
        .await
        .map_err(|error| {
            tracing::warn!(%error, "client auth verification task failed");
            error
        })
        .ok()
        .flatten();
        if let Some(key_id) = &matched {
            let mut cache = self.cache.lock().unwrap_or_else(|e| {
                tracing::warn!("client auth digest cache poisoned; recovering from panic");
                e.into_inner()
            });
            if !cache
                .iter()
                .any(|entry| constant_time_eq(&entry.digest, &digest))
            {
                cache.push(CachedDigest {
                    digest,
                    key_id: key_id.clone(),
                });
            }
        }
        matched
    }

    /// Authenticate a bearer token, returning the matching key id.
    ///
    /// Cache hits skip the KDF; misses verify against every configured key on
    /// the blocking pool. `None` means the token is not a configured client
    /// key (or auth is not enforced, in which case nothing authenticates).
    pub(crate) async fn authenticate(&self, token: &str) -> Option<String> {
        if !self.enforced || self.verifiers.is_empty() {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(key_id) = self.cached_key_id(&digest) {
            return Some(key_id);
        }
        self.verify_uncached(token).await
    }
}

/// Extract the bearer token from `Authorization: Bearer <token>`.
///
/// The scheme is matched case-insensitively (RFC 7235). Returns `None` when
/// the header is absent, uses another scheme, or carries an empty token. The
/// returned secret is never logged; holders use it only for verification.
pub(crate) fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Option<SecretString> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let header = value.to_str().ok()?;
    let mut parts = header.splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some(scheme), Some(token)) if scheme.eq_ignore_ascii_case("bearer") => {
            let token = token.trim();
            if token.is_empty() {
                None
            } else {
                Some(SecretString::from(token.to_owned()))
            }
        }
        _ => None,
    }
}

/// Typed caller identity carried in request extensions.
///
/// Inserted by the client-auth middleware on proxy routes: the authenticated
/// key id when client keys are enforced, or [`CallerId::anonymous`] under the
/// loopback no-auth exemption. Handlers read it for caller attribution on
/// events and for rate-limit keying.
#[derive(Debug, Clone)]
pub struct CallerId {
    /// Authenticated client key id; `None` when unauthenticated (no-auth
    /// mode).
    key_id: Option<String>,
}

impl CallerId {
    /// The unauthenticated caller used under the loopback no-auth exemption.
    #[must_use]
    pub fn anonymous() -> Self {
        Self { key_id: None }
    }

    /// An authenticated caller identified by `key_id`.
    #[must_use]
    pub fn authenticated(key_id: impl Into<String>) -> Self {
        Self {
            key_id: Some(key_id.into()),
        }
    }

    /// Whether a client key authenticated this request.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.key_id.is_some()
    }

    /// The attribution string for events and logs: the key id, or
    /// `"anonymous"` when unauthenticated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.key_id.as_deref().unwrap_or(ANONYMOUS_CALLER)
    }
}

impl std::fmt::Display for CallerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extractor bundling the per-request identity stamped by the outer middleware
/// layers: the injected [`RequestId`], the [`CallerId`] from the client-auth
/// layer, and the peer address (when connection info is available).
///
/// One extractor replaces three in every proxy handler signature, and the
/// owned struct is passed whole into inner handlers and
/// [`prepare_request`](crate::routes::core_pipeline::prepare_request).
///
/// Absent extensions degrade the same way the individual extractors would:
/// a missing request id becomes `"unknown"` and a missing caller becomes
/// [`CallerId::anonymous`] (defense-in-depth for routers built without the
/// usual layers; through [`crate::routes::router`] both are always present).
#[derive(Debug, Clone)]
pub(crate) struct RequestMeta {
    /// Request id minted by the outermost `inject_request_id` layer.
    pub(crate) request_id: RequestId,
    /// Caller identity stamped by the client-auth layer.
    pub(crate) caller: CallerId,
    /// Peer socket address, when the server was built with connection info.
    pub(crate) connect_info: Option<SocketAddr>,
}

impl<S> FromRequestParts<S> for RequestMeta
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let request_id = parts
            .extensions
            .get::<RequestId>()
            .cloned()
            .unwrap_or_else(|| RequestId("unknown".to_owned()));
        let caller = parts
            .extensions
            .get::<CallerId>()
            .cloned()
            .unwrap_or_else(CallerId::anonymous);
        let connect_info = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|connect_info| connect_info.0);
        async move {
            Ok(Self {
                request_id,
                caller,
                connect_info,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ClientAuth ------------------------------------------------------------

    /// Hash a token with deliberately cheap argon2id parameters (8 KiB, one
    /// pass) so tests run in microseconds; `ClientAuth` verification reads the
    /// cost parameters from the PHC string, so verification is equally cheap.
    fn cheap_phc(token: &str, salt: &[u8]) -> String {
        let params = argon2::Params::new(8, 1, 1, None).expect("valid test params");
        let hasher = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            params.clone(),
        );
        use argon2::CustomizedPasswordHasher;
        hasher
            .hash_password_customized(
                token.as_bytes(),
                salt,
                Some(&argon2::ARGON2ID_IDENT),
                None,
                params,
            )
            .expect("hash token")
            .to_string()
    }

    fn auth_config_with(phc: &str) -> llm_proxy_core::AuthConfig {
        let mut clients = HashMap::new();
        clients.insert("laptop".to_owned(), SecretString::from(phc.to_owned()));
        llm_proxy_core::AuthConfig {
            mode: llm_proxy_core::AuthMode::ClientKeys,
            clients,
        }
    }

    #[test]
    fn client_auth_from_config_rejects_mode_contradictions() {
        let none_with_clients = {
            let mut clients = HashMap::new();
            clients.insert(
                "laptop".to_owned(),
                SecretString::from(cheap_phc("secret", b"0123456789abcdef")),
            );
            llm_proxy_core::AuthConfig {
                mode: llm_proxy_core::AuthMode::None,
                clients,
            }
        };
        assert!(matches!(
            ClientAuth::from_config(&none_with_clients),
            Err(ClientAuthError::ModeNoneWithClients)
        ));

        let keys_without_clients = llm_proxy_core::AuthConfig {
            mode: llm_proxy_core::AuthMode::ClientKeys,
            clients: HashMap::new(),
        };
        assert!(matches!(
            ClientAuth::from_config(&keys_without_clients),
            Err(ClientAuthError::ClientKeysWithoutClients)
        ));
    }

    #[test]
    fn client_auth_from_config_rejects_bad_key_ids_and_hashes() {
        for bad_id in ["", "anonymous"] {
            let mut config = auth_config_with(&cheap_phc("secret", b"0123456789abcdef"));
            config.clients.remove("laptop");
            config
                .clients
                .insert(bad_id.to_owned(), SecretString::from("x".to_owned()));
            assert!(
                matches!(
                    ClientAuth::from_config(&config),
                    Err(ClientAuthError::InvalidKeyId(id)) if id == bad_id
                ),
                "key id {bad_id:?} must be rejected"
            );
        }

        let config = auth_config_with("not-a-phc-string");
        let error = ClientAuth::from_config(&config).expect_err("malformed verifier rejected");
        assert!(
            error.to_string().contains("laptop"),
            "error names the key id, got: {error}"
        );
        assert!(
            !error.to_string().contains("not-a-phc-string"),
            "error must not echo the verifier, got: {error}"
        );
    }

    #[tokio::test]
    async fn client_auth_authenticates_a_configured_key_and_rejects_others() {
        let secret = cheap_phc("the-real-secret", b"0123456789abcdef");
        let auth = ClientAuth::from_config(&auth_config_with(&secret)).expect("valid config");

        let matched = auth.authenticate("the-real-secret").await;
        assert_eq!(matched.as_deref(), Some("laptop"));
        // Repeat hits the digest cache and must still resolve.
        assert_eq!(
            auth.authenticate("the-real-secret").await.as_deref(),
            Some("laptop")
        );
        assert_eq!(auth.authenticate("wrong-secret").await, None);
        assert_eq!(auth.authenticate("").await, None);
    }

    #[tokio::test]
    async fn client_auth_picks_the_matching_key_among_many() {
        let mut clients = HashMap::new();
        for (id, token) in [("alpha", "one"), ("beta", "two"), ("gamma", "three")] {
            clients.insert(
                id.to_owned(),
                SecretString::from(cheap_phc(token, b"0123456789abcdef")),
            );
        }
        let config = llm_proxy_core::AuthConfig {
            mode: llm_proxy_core::AuthMode::ClientKeys,
            clients,
        };
        let auth = ClientAuth::from_config(&config).expect("valid config");
        assert_eq!(auth.authenticate("two").await.as_deref(), Some("beta"));
        assert_eq!(auth.authenticate("three").await.as_deref(), Some("gamma"));
        assert_eq!(auth.authenticate("four").await, None);
    }

    #[test]
    fn client_auth_none_mode_is_not_enforced() {
        let auth =
            ClientAuth::from_config(&llm_proxy_core::AuthConfig::default()).expect("valid config");
        assert!(!auth.is_enforced());
    }

    #[test]
    fn client_auth_debug_shows_key_ids_but_never_verifiers() {
        let secret = cheap_phc("the-real-secret", b"0123456789abcdef");
        let auth = ClientAuth::from_config(&auth_config_with(&secret)).expect("valid config");
        let debug = format!("{auth:?}");
        assert!(debug.contains("laptop"), "got: {debug}");
        assert!(
            !debug.contains("argon2id") && !debug.contains(&secret),
            "verifier string must not appear in Debug, got: {debug}"
        );
    }

    #[test]
    fn constant_time_eq_behaves_as_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    // -- extract_bearer_token --------------------------------------------------

    #[test]
    fn bearer_token_extracts_case_insensitive_scheme_and_trims() {
        use axum::http::HeaderMap;
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(
            extract_bearer_token(&headers).map(|t| t.expose_secret().to_owned()),
            Some("abc123".to_owned())
        );

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "bearer abc123 ".parse().unwrap());
        assert_eq!(
            extract_bearer_token(&headers).map(|t| t.expose_secret().to_owned()),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn bearer_token_rejects_missing_other_scheme_and_empty() {
        use axum::http::HeaderMap;
        // SecretString has no PartialEq (deliberately); assert via Option::is_none.
        assert!(extract_bearer_token(&HeaderMap::new()).is_none());

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(extract_bearer_token(&headers).is_none());

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer   ".parse().unwrap());
        assert!(extract_bearer_token(&headers).is_none());
    }

    // -- CallerId ---------------------------------------------------------------

    #[test]
    fn caller_id_attribution_strings() {
        assert_eq!(CallerId::anonymous().as_str(), "anonymous");
        assert_eq!(CallerId::authenticated("laptop").as_str(), "laptop");
        assert!(!CallerId::anonymous().is_authenticated());
        assert!(CallerId::authenticated("laptop").is_authenticated());
        assert_eq!(CallerId::authenticated("laptop").to_string(), "laptop");
    }

    // -- RequestDeduplicator ---------------------------------------------------

    #[test]
    fn dedup_new_request_is_not_duplicate() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
    }

    #[test]
    fn dedup_same_body_is_duplicate() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
        assert!(dedup.is_duplicate_with_path("", b"hello"));
    }

    #[test]
    fn dedup_different_body_is_not_duplicate() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
        assert!(!dedup.is_duplicate_with_path("", b"world"));
    }

    #[test]
    fn request_id_format() {
        let generator = RequestIdGenerator::new();
        let id = generator.next_id();
        assert!(id.starts_with("req-"));
        assert!(id.contains('-'));
    }

    #[test]
    fn request_id_monotonically_increasing() {
        let generator = RequestIdGenerator::new();
        let id1 = generator.next_id();
        let id2 = generator.next_id();
        // The counter portion should be different.
        assert_ne!(id1, id2);
    }

    // -- RateLimiter -----------------------------------------------------------

    #[test]
    fn rate_limiter_allows_under_limit() {
        let limiter = RateLimiter::new(100);
        for _ in 0..10 {
            assert!(limiter.is_allowed("127.0.0.1"));
        }
    }

    #[test]
    fn rate_limiter_different_ips_independent() {
        let limiter = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.is_allowed("10.0.0.1"));
        }
        // Different IP should still be allowed.
        assert!(limiter.is_allowed("10.0.0.2"));
    }

    // -- RequestDeduplicator with path -----------------------------------------

    #[test]
    fn dedup_same_body_different_path_is_not_duplicate() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate_with_path("/v1/messages", b"hello"));
        // Same body, different path -> not a duplicate.
        assert!(!dedup.is_duplicate_with_path("/v1/chat/completions", b"hello"));
    }

    #[test]
    fn dedup_same_body_same_path_is_duplicate() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate_with_path("/v1/messages", b"hello"));
        assert!(dedup.is_duplicate_with_path("/v1/messages", b"hello"));
    }

    #[test]
    fn dedup_custom_window() {
        let dedup = RequestDeduplicator::with_window_ms(1000);
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
        assert!(dedup.is_duplicate_with_path("", b"hello"));
    }

    #[test]
    fn dedup_zero_window_is_disabled() {
        let dedup = RequestDeduplicator::with_window_ms(0);
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
        assert!(!dedup.is_duplicate_with_path("", b"hello"));
    }

    /// Exercise the deprecated path-less shim so the public entry point cannot
    /// break silently. `#[expect(deprecated)]` (rather than `#[allow]`) surfaces
    /// an `unfulfilled_lint_expectation` warning the day the shim is removed.
    #[test]
    #[expect(deprecated, reason = "is_duplicate shim is exercised directly here")]
    fn dedup_path_less_shim_still_works() {
        let dedup = RequestDeduplicator::new();
        assert!(!dedup.is_duplicate(b"hello"));
        // Identical body seen twice under the empty path is still a duplicate.
        assert!(dedup.is_duplicate(b"hello"));
    }

    // -- get_client_ip ---------------------------------------------------------

    #[test]
    fn client_ip_trust_forwarded_headers_false_ignores_headers() {
        use axum::http::HeaderMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let connect_info = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        let ip = super::get_client_ip(&headers, Some(&connect_info), false);
        assert_eq!(
            ip, "127.0.0.1",
            "should ignore X-Forwarded-For when trust=false"
        );
    }

    #[test]
    fn client_ip_trust_forwarded_headers_true_uses_headers() {
        use axum::http::HeaderMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let connect_info = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        let ip = super::get_client_ip(&headers, Some(&connect_info), true);
        assert_eq!(ip, "1.2.3.4", "should use X-Forwarded-For when trust=true");
    }

    #[test]
    fn rate_limiter_zero_limit_is_disabled() {
        let limiter = RateLimiter::new(0);
        for _ in 0..1_000 {
            assert!(limiter.is_allowed("127.0.0.1"));
        }
    }

    // -- RateLimiter::would_allow (auth flood gate) -----------------------------

    #[test]
    fn would_allow_is_false_once_the_peer_bucket_is_drained() {
        let limiter = RateLimiter::new(1);
        assert!(
            limiter.is_allowed("10.0.0.9"),
            "first request consumes the only token"
        );
        // Refill is 1 token/minute, so within the test's lifetime the drained
        // bucket stays empty and the peek must report denial.
        assert!(
            !limiter.would_allow("10.0.0.9"),
            "drained peer must be denied by the peek"
        );
        // Other peers are unaffected: a fresh bucket starts full.
        assert!(limiter.would_allow("10.0.0.8"));
    }

    #[test]
    fn would_allow_reports_true_for_unknown_peer_and_disabled_limiter() {
        let limiter = RateLimiter::new(100);
        assert!(
            limiter.would_allow("10.0.0.7"),
            "unknown peer starts with a full bucket"
        );

        let disabled = RateLimiter::new(0);
        for _ in 0..50 {
            assert!(disabled.is_allowed("10.0.0.7"));
        }
        assert!(
            disabled.would_allow("10.0.0.7"),
            "a disabled limiter never denies"
        );
    }

    #[test]
    fn would_allow_does_not_consume_tokens() {
        let limiter = RateLimiter::new(1);
        assert!(limiter.is_allowed("10.0.0.6"));
        // Repeated peeks must neither drain nor refill the bucket: the peer
        // stays denied and a subsequent consume attempt is also denied.
        assert!(!limiter.would_allow("10.0.0.6"));
        assert!(!limiter.would_allow("10.0.0.6"));
        assert!(!limiter.is_allowed("10.0.0.6"));
    }

    #[test]
    fn rate_limiter_rejects_requests_at_the_limit() {
        // With 1 RPM, the first request should be allowed and the second rejected.
        let limiter = RateLimiter::new(1);
        assert!(
            limiter.is_allowed("10.0.0.1"),
            "first request should be allowed"
        );
        assert!(
            !limiter.is_allowed("10.0.0.1"),
            "second request should be rejected at 1 RPM limit"
        );
    }

    #[test]
    fn dedup_large_body_is_not_tracked() {
        let dedup = RequestDeduplicator::with_window_ms(5000);
        let large_body = vec![b'x'; 65 * 1024]; // 65 KiB > 64 KiB cap
        // Large bodies should never be tracked as duplicates.
        assert!(!dedup.is_duplicate_with_path("/v1/messages", &large_body));
        assert!(
            !dedup.is_duplicate_with_path("/v1/messages", &large_body),
            "large body should not be tracked for dedup"
        );
    }

    #[test]
    fn dedup_body_at_cap_is_still_tracked() {
        let dedup = RequestDeduplicator::with_window_ms(5000);
        let body_at_cap = vec![b'x'; 64 * 1024]; // Exactly at 64 KiB cap
        assert!(!dedup.is_duplicate_with_path("/v1/messages", &body_at_cap));
        assert!(
            dedup.is_duplicate_with_path("/v1/messages", &body_at_cap),
            "body at exactly the cap should still be tracked"
        );
    }

    // -- Multi-value X-Forwarded-For (finding 24) ------------------------------

    #[test]
    fn client_ip_multi_value_xff_picks_leftmost() {
        use axum::http::HeaderMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        let connect_info = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        let ip = super::get_client_ip(&headers, Some(&connect_info), true);
        assert_eq!(
            ip, "1.2.3.4",
            "should pick the leftmost IP from multi-value XFF"
        );
    }

    // -- get_client_ip returns 'unknown' (finding 26) --------------------------

    #[test]
    fn client_ip_returns_unknown_when_no_connect_info_and_no_headers() {
        use axum::http::HeaderMap;

        let headers = HeaderMap::new();
        let ip = super::get_client_ip(&headers, None, false);
        assert_eq!(
            ip, "unknown",
            "should return 'unknown' when no IP source is available"
        );
    }

    #[test]
    fn client_ip_trusted_xff_with_invalid_ip_falls_through() {
        use axum::http::HeaderMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let connect_info = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 8080);

        let ip = super::get_client_ip(&headers, Some(&connect_info), true);
        assert_eq!(
            ip, "10.0.0.1",
            "invalid XFF value should fall through to connect_info"
        );
    }

    #[test]
    fn client_ip_x_real_ip_is_used_when_xff_absent() {
        use axum::http::HeaderMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "9.8.7.6".parse().unwrap());
        let connect_info = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        let ip = super::get_client_ip(&headers, Some(&connect_info), true);
        assert_eq!(ip, "9.8.7.6", "should use X-Real-Ip when XFF is absent");
    }
}
