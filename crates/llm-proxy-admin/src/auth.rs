//! The admin bearer-token check (the `[auth.admin]` token): SHA-256
//! digests compared in constant time, so the plaintext is never retained.
//! The MCP endpoint authenticates through the same check.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, middleware::Next};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(crate) struct AdminAuth {
    expected_digest: [u8; 32],
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAuth")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl AdminAuth {
    pub(crate) fn new(token: &str) -> Self {
        Self {
            expected_digest: Sha256::digest(token.as_bytes()).into(),
        }
    }

    pub(crate) fn accepts(&self, presented: Option<&str>) -> bool {
        let Some(token) = presented else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        constant_time_eq(&digest, &self.expected_digest)
    }
}

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

pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
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

#[derive(Serialize)]
struct UnauthorizedBody {
    error: UnauthorizedError,
}

#[derive(Serialize)]
struct UnauthorizedError {
    r#type: &'static str,
    message: &'static str,
}

pub(crate) async fn require_admin(
    auth: State<Arc<AdminAuth>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if auth.accepts(bearer_token(req.headers())) {
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

/// `Cache-Control: no-store` (+ `Pragma` for HTTP/1.0 caches) on every
/// admin response: an intermediary must never cache authenticated admin
/// data (backlog 37).
pub(crate) const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

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
    fn debug_never_prints_the_token() {
        let auth = AdminAuth::new("admin-secret");
        let debug = format!("{auth:?}");
        assert!(
            !debug.contains("admin-secret"),
            "Debug must not expose the token, got: {debug}"
        );
    }
}
