use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, middleware::Next};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::routes::UsageState;

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
