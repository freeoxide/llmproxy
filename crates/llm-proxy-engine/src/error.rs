//! The engine's error type: transport- and HTTP-free, mapped by the server
//! crate into client-facing status codes and bodies.

use std::time::Duration;

use llm_proxy_adapters::ProviderError;
use llm_proxy_core::ProviderRouteResolutionError;

use crate::classification::rate_limit_reset;

/// Who owns the credential an upstream auth failure implicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOwner {
    /// The client's token (passthrough auth): the client can fix it.
    Client,
    /// The operator's configured upstream key: only the operator can fix it.
    Operator,
}

/// Failures produced while running a request through the engine. Statuses
/// are plain `u16`s; the HTTP layer decides how to render them.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The provider name in the URL is malformed.
    #[error("{0}")]
    InvalidProviderName(String),
    /// No provider is configured under this name.
    #[error("unknown provider \"{0}\"")]
    UnknownProvider(String),
    /// The provider does not serve this endpoint family.
    #[error("{0}")]
    UnsupportedRoute(String),
    /// Catalog enforcement rejected the resolved model.
    #[error("model \"{0}\" is not allowed for this provider")]
    ModelNotAllowed(String),
    /// Passthrough auth requires a client credential and none arrived.
    #[error("unauthorized")]
    Unauthorized,
    /// Admission (budget/limits reservation) refused the request.
    #[error("{0}")]
    RateLimited(String),
    /// The client request is not representable (bad body or cross-format
    /// field rejection naming the fields).
    #[error("{0}")]
    InvalidRequest(String),
    /// The upstream response could not be decoded.
    #[error("{0}")]
    ProviderDecode(String),
    /// The upstream returned an error status. `body` is already sanitized.
    #[error("upstream error {status}")]
    Upstream {
        /// Upstream HTTP status.
        status: u16,
        /// Sanitized upstream body excerpt.
        body: String,
        /// Whose credential an auth failure implicates.
        auth_owner: AuthOwner,
        /// The upstream-advised wait parsed from `Retry-After` or a
        /// rate-limit reset header, when the error response carried one.
        retry_after: Option<Duration>,
    },
    /// The upstream timed out.
    #[error("upstream request timed out: {0}")]
    UpstreamTimeout(String),
    /// The retry/failover ladder ran out of attempts (plan 04 step 12,
    /// backlog 38): the proxy already tried, so the client must not stack
    /// its own retry loop on top. Status and message delegate to the last
    /// underlying error.
    #[error("retries exhausted after {attempts} attempts: {last}")]
    RetryExhausted {
        /// Attempts the ladder made, including the failed last one.
        attempts: u32,
        /// The error the final attempt produced.
        last: Box<EngineError>,
        /// The wait after which another request might succeed: the
        /// upstream-advised or cooldown-derived reset, when known.
        retry_after: Option<Duration>,
    },
    /// The route's affinity policy refused a failover (plan 04 step 10,
    /// conflict 6): the session is pinned to a member and switching would
    /// carry state bound to another account.
    #[error("session is pinned to provider \"{pinned}\" and the failover policy refuses to switch")]
    AffinityRefused {
        /// The member the session is pinned to.
        pinned: String,
        /// The session id.
        session: String,
    },
    /// An internal invariant broke.
    #[error("internal error: {0}")]
    Internal(String),
}

impl EngineError {
    /// Builds from a route-resolution failure.
    #[must_use]
    pub fn from_route_error(error: ProviderRouteResolutionError) -> Self {
        match error {
            ProviderRouteResolutionError::UnknownProvider { provider } => {
                Self::UnknownProvider(provider)
            }
            error @ ProviderRouteResolutionError::UnsupportedRoute { .. } => {
                Self::UnsupportedRoute(error.to_string())
            }
            error => Self::Internal(error.to_string()),
        }
    }

    /// The `error_kind` string recorded on usage rows.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InvalidProviderName(_) => "InvalidProviderName",
            Self::UnknownProvider(_) => "UnknownProvider",
            Self::UnsupportedRoute(_) => "UnsupportedRoute",
            Self::ModelNotAllowed(_) => "ModelNotAllowed",
            Self::Unauthorized => "Unauthorized",
            Self::RateLimited(_) => "RateLimited",
            Self::InvalidRequest(_) => "InvalidRequest",
            Self::ProviderDecode(_) => "ProviderDecode",
            Self::Upstream { .. } => "Upstream",
            Self::UpstreamTimeout(_) => "UpstreamTimeout",
            Self::RetryExhausted { .. } => "RetryExhausted",
            Self::AffinityRefused { .. } => "AffinityRefused",
            Self::Internal(_) => "Internal",
        }
    }

    /// The HTTP status the server maps this to.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Upstream {
                status, auth_owner, ..
            } => map_upstream_status(*status, *auth_owner),
            Self::InvalidProviderName(_) => 400,
            Self::UnknownProvider(_) => 404,
            Self::UnsupportedRoute(_) => 404,
            Self::ModelNotAllowed(_) => 400,
            Self::Unauthorized => 401,
            Self::RateLimited(_) => 429,
            Self::InvalidRequest(_) => 400,
            Self::ProviderDecode(_) => 502,
            Self::UpstreamTimeout(_) => 504,
            Self::RetryExhausted { last, .. } => last.status_code(),
            Self::AffinityRefused { .. } => 503,
            Self::Internal(_) => 500,
        }
    }

    /// The upstream-advised or ladder-computed wait, when this error carries
    /// one (the value the server renders as `Retry-After`).
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Upstream { retry_after, .. } => *retry_after,
            Self::RetryExhausted { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Whether the proxy wants the client to retry this request itself.
    /// `false` exactly when the ladder already exhausted its own retries
    /// (backlog 38): the server renders this as `x-should-retry`.
    #[must_use]
    pub fn should_retry(&self) -> bool {
        !matches!(self, Self::RetryExhausted { .. })
    }

    /// Sanitized client-facing message.
    #[must_use]
    pub fn client_message(&self) -> String {
        match self {
            Self::Upstream { body, .. } => body.clone(),
            other => other.to_string(),
        }
    }
}

/// The one upstream-status policy (audit conflict 14): what the proxy maps
/// each upstream status to. `400`/`413`/`429` pass through (the client can
/// fix them), `401`/`403` pass through only when the client's own credential
/// is at stake, everything else collapses to `502` so the upstream's own
/// error schema never reaches the client.
#[must_use]
pub fn map_upstream_status(upstream: u16, auth_owner: AuthOwner) -> u16 {
    match upstream {
        400 | 413 | 429 => upstream,
        401 | 403 if matches!(auth_owner, AuthOwner::Client) => upstream,
        _ => 502,
    }
}

/// Maps an adapter/transport error to an engine error, given who owns the
/// credential. `Api` errors keep their response headers' reset advice as
/// `retry_after`.
#[must_use]
pub fn map_provider_error(error: &ProviderError, passthrough_auth: bool) -> EngineError {
    let auth_owner = if passthrough_auth {
        AuthOwner::Client
    } else {
        AuthOwner::Operator
    };
    match error {
        ProviderError::Api {
            status,
            body,
            headers,
        } => EngineError::Upstream {
            status: *status,
            body: body.clone(),
            auth_owner,
            retry_after: retry_after_from_headers(headers),
        },
        other => {
            if other.is_timeout() {
                EngineError::UpstreamTimeout(fully_sanitize(other))
            } else {
                EngineError::Upstream {
                    status: 502,
                    body: fully_sanitize(other),
                    auth_owner,
                    retry_after: None,
                }
            }
        }
    }
}

/// The wait advised by an error response's headers (lowercased names):
/// `Retry-After` and the rate-limit reset dialects, longest window wins.
#[must_use]
pub fn retry_after_from_headers(headers: &[(String, String)]) -> Option<std::time::Duration> {
    rate_limit_reset(headers)
}

const MAX_SANITIZE_LEN: usize = 512;
const SANITIZE_SUFFIX: &str = "...[truncated]";
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time-constant regex literal; a failure is a programming \
              error caught the moment this static is first used, not a runtime input"
)]
static URL_REDACT_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"https?://\S+").expect("valid URL redaction regex")
});

fn sanitize_with_len_limit(msg: &str) -> String {
    let truncated =
        llm_proxy_protocol::util::truncate_with_suffix(msg, MAX_SANITIZE_LEN, SANITIZE_SUFFIX);
    URL_REDACT_REGEX
        .replace_all(&truncated, "[url-redacted]")
        .into_owned()
}

/// Truncates and redacts URLs from an upstream error message.
#[must_use]
pub fn sanitize_upstream_error_body(msg: &str) -> String {
    sanitize_with_len_limit(msg)
}

/// Truncates, redacts URLs, and strips key-shaped tokens from an upstream
/// error message, by routing it through the adapter error sanitizer.
#[must_use]
pub fn fully_sanitize_upstream_error(msg: &str) -> String {
    match ProviderError::api(0, msg.to_owned()) {
        ProviderError::Api { body, .. } => sanitize_with_len_limit(&body),
        _ => sanitize_with_len_limit(msg),
    }
}

fn fully_sanitize(error: &ProviderError) -> String {
    fully_sanitize_upstream_error(&error.to_string())
}
