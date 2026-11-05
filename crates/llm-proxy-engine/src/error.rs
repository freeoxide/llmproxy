//! The engine's error type: transport- and HTTP-free, mapped by the server
//! crate into client-facing status codes and bodies.

use llm_proxy_adapters::ProviderError;
use llm_proxy_core::ProviderRouteResolutionError;

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
    },
    /// The upstream timed out.
    #[error("upstream request timed out: {0}")]
    UpstreamTimeout(String),
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
            Self::Internal(_) => 500,
        }
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
/// credential.
#[must_use]
pub fn map_provider_error(error: &ProviderError, passthrough_auth: bool) -> EngineError {
    let auth_owner = if passthrough_auth {
        AuthOwner::Client
    } else {
        AuthOwner::Operator
    };
    match error {
        ProviderError::Api { status, body } => EngineError::Upstream {
            status: *status,
            body: body.clone(),
            auth_owner,
        },
        other => {
            if other.is_timeout() {
                EngineError::UpstreamTimeout(fully_sanitize(other))
            } else {
                EngineError::Upstream {
                    status: 502,
                    body: fully_sanitize(other),
                    auth_owner,
                }
            }
        }
    }
}

const MAX_SANITIZE_LEN: usize = 512;
const SANITIZE_SUFFIX: &str = "...[truncated]";
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
