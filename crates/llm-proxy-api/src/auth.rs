//! Admin bearer-token authentication for the `/usage` surface.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, middleware::Next};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::routes::UsageState;

/// Guard for the admin token: stores only its SHA-256 digest.
///
/// The configured token is hashed once at construction and never retained,
/// so neither the plaintext nor anything it could be compared against
/// directly survives in memory. Presented tokens are hashed the same way and
/// the two fixed-length digests are compared in constant time, which also
/// removes any length-dependence from the comparison.
#[derive(Clone)]
pub(crate) struct AdminAuth {
    expected_digest: [u8; 32],
}

impl std::fmt::Debug for AdminAuth {
    // The digest is offline-brute-forceable for low-entropy tokens (SHA-256
    // preimage search), so it is treated as sensitive and never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAuth")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl AdminAuth {
    /// Build the guard from the configured admin token.
    pub(crate) fn new(token: &str) -> Self {
        Self {
            expected_digest: Sha256::digest(token.as_bytes()).into(),
        }
    }

    /// Whether `presented` (a bearer token already extracted from the
    /// Authorization header) is the configured admin token.
    pub(crate) fn accepts(&self, presented: Option<&str>) -> bool {
        let Some(token) = presented else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        constant_time_eq(&digest, &self.expected_digest)
    }
}

/// Constant-time equality of two equal-length byte strings.
///
/// Compares all bytes unconditionally so the runtime does not depend on
/// where the first difference appears. Both inputs here are fixed 32-byte
/// digests, so the length branch is not secret-dependent.
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

/// Extract the bearer token from `Authorization: Bearer <token>`.
///
/// The scheme is matched case-insensitively (RFC 7235). Returns `None` when
/// the header is absent, uses another scheme, or carries an empty token.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?;
    let header_value = value.to_str().ok()?;
    let mut parts = header_value.splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some(scheme), Some(token)) if scheme.eq_ignore_ascii_case("bearer") => {
            let token = token.trim();
            (!token.is_empty()).then_some(token)
        }
        _ => None,
    }
}

/// Body of every admin-auth rejection: static text that never echoes any
/// part of the presented credentials.
#[derive(Serialize)]
struct UnauthorizedBody {
    error: UnauthorizedError,
}

#[derive(Serialize)]
struct UnauthorizedError {
    r#type: &'static str,
    message: &'static str,
}

/// Middleware requiring the admin token on every `/usage` route.
///
/// Every rejection produces the same uniform 401 body, so the response does
/// not reveal whether a token was missing, malformed, or merely wrong. The
/// token is never logged and never appears in any response.
pub(crate) async fn require_admin(
    State(state): State<UsageState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if state.admin.accepts(bearer_token(req.headers())) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(UnauthorizedBody {
            error: UnauthorizedError {
                r#type: "unauthorized",
                message: "missing or invalid admin token",
            },
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_configured_token() {
        let auth = AdminAuth::new("admin-secret");
        assert!(auth.accepts(Some("admin-secret")));
        assert!(!auth.accepts(Some("admin-secret2")));
        assert!(!auth.accepts(Some("")));
        assert!(!auth.accepts(None));
    }

    #[test]
    fn debug_never_prints_the_digest() {
        let auth = AdminAuth::new("admin-secret");
        let debug = format!("{auth:?}");
        assert!(
            !debug.contains("admin-secret"),
            "Debug must not expose the token, got: {debug}"
        );
    }

    #[test]
    fn bearer_token_extraction() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer tok".parse().expect("valid header value"),
        );
        assert_eq!(bearer_token(&headers), Some("tok"));
        headers.insert(
            header::AUTHORIZATION,
            "bearer tok".parse().expect("valid header value"),
        );
        assert_eq!(
            bearer_token(&headers),
            Some("tok"),
            "scheme matching is case-insensitive"
        );
        headers.insert(
            header::AUTHORIZATION,
            "Basic dXNlcjpwYXNz".parse().expect("valid header value"),
        );
        assert_eq!(bearer_token(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer ".parse().expect("valid header value"),
        );
        assert_eq!(bearer_token(&headers), None, "empty token is no token");
    }
}
