use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(super) enum ClientProtocol {
    Anthropic,
    OpenAiChat,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum AuthOwner {
    #[default]
    Operator,
    Client,
}
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(super) enum RouteError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("model not allowed: {0}")]
    ModelNotAllowed(String),
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("unsupported route: {0}")]
    UnsupportedRoute(String),
    #[error("invalid provider name: {0}")]
    InvalidProviderName(String),
    #[error("upstream error: {status}")]
    Upstream {
        status: StatusCode,
        body: String,
        auth_owner: AuthOwner,
    },
    #[error("upstream timeout: {0}")]
    UpstreamTimeout(String),
    #[error("provider decode error: {0}")]
    ProviderDecode(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("rate limit exceeded")]
    RateLimited,
    #[error("duplicate request")]
    Conflict,
    #[error("not found")]
    NotFound,
    #[error("request timeout")]
    RequestTimeout,
    #[error("request body too large")]
    PayloadTooLarge,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("missing client auth token for passthrough provider")]
    Unauthorized,
}
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct AnthropicErrorBody {
    r#type: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct AnthropicErrorDetail {
    r#type: String,
    message: String,
}
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct OpenAiErrorBody {
    error: OpenAiErrorDetail,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct OpenAiErrorDetail {
    message: String,
    r#type: String,
    code: serde_json::Value,
}
pub(super) fn route_error_response(protocol: ClientProtocol, error: &RouteError) -> Response<Body> {
    match protocol {
        ClientProtocol::Anthropic => anthropic_error_response(error),
        ClientProtocol::OpenAiChat => openai_error_response(error),
    }
}
pub(crate) const INTERNAL_ERROR_CLIENT_MESSAGE: &str = "internal server error";
pub(crate) const PROVIDER_DECODE_CLIENT_MESSAGE: &str = "provider response decode error";
pub(crate) const UPSTREAM_PASSTHROUGH_CLIENT_MESSAGE: &str = "upstream rejected the request";
pub(crate) const UPSTREAM_COLLAPSE_CLIENT_MESSAGE: &str = "upstream service error";
fn anthropic_error_response(error: &RouteError) -> Response<Body> {
    let (status, error_type, message) = extract_error_fields(error);

    let body = AnthropicErrorBody {
        r#type: "error",
        error: AnthropicErrorDetail {
            r#type: error_type.to_owned(),
            message,
        },
    };
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}
fn openai_error_response(error: &RouteError) -> Response<Body> {
    let (status, error_type, message) = extract_error_fields(error);
    let error_type = match status {
        StatusCode::INTERNAL_SERVER_ERROR => "server_error",
        StatusCode::NOT_FOUND => "invalid_request_error",
        _ => error_type,
    };

    let body = OpenAiErrorBody {
        error: OpenAiErrorDetail {
            message,
            r#type: error_type.to_owned(),
            code: serde_json::Value::Null,
        },
    };
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}
pub(super) fn openai_stream_error_json_with_type(
    message: &str,
    error_type: &str,
) -> Option<String> {
    let body = OpenAiErrorBody {
        error: OpenAiErrorDetail {
            message: truncate_error_body(message),
            r#type: error_type.to_owned(),
            code: serde_json::Value::Null,
        },
    };
    serde_json::to_string(&body).ok()
}
pub(crate) fn route_error_variant_name(error: &RouteError) -> &'static str {
    match error {
        RouteError::InvalidRequest(_) => "InvalidRequest",
        RouteError::ModelNotAllowed(_) => "ModelNotAllowed",
        RouteError::UnknownProvider(_) => "UnknownProvider",
        RouteError::UnsupportedRoute(_) => "UnsupportedRoute",
        RouteError::InvalidProviderName(_) => "InvalidProviderName",
        RouteError::Upstream { .. } => "Upstream",
        RouteError::UpstreamTimeout(_) => "UpstreamTimeout",
        RouteError::ProviderDecode(_) => "ProviderDecode",
        RouteError::Internal(_) => "Internal",
        RouteError::RateLimited => "RateLimited",
        RouteError::Conflict => "Conflict",
        RouteError::NotFound => "NotFound",
        RouteError::RequestTimeout => "RequestTimeout",
        RouteError::PayloadTooLarge => "PayloadTooLarge",
        RouteError::MethodNotAllowed => "MethodNotAllowed",
        RouteError::Unauthorized => "Unauthorized",
    }
}
pub(crate) fn extract_error_fields(error: &RouteError) -> (StatusCode, &'static str, String) {
    match error {
        RouteError::InvalidRequest(msg) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            msg.clone(),
        ),
        RouteError::ModelNotAllowed(model) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("model not allowed: {model}"),
        ),
        RouteError::UnknownProvider(provider) => (
            StatusCode::NOT_FOUND,
            "not_found_error",
            format!("unknown provider: {provider}"),
        ),
        RouteError::UnsupportedRoute(msg) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            msg.clone(),
        ),
        RouteError::InvalidProviderName(msg) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("invalid provider name: {msg}"),
        ),
        RouteError::Upstream {
            status,
            body: _body,
            auth_owner,
        } => {
            let mapped = map_upstream_status(*status, *auth_owner);
            let message = if mapped == *status {
                UPSTREAM_PASSTHROUGH_CLIENT_MESSAGE
            } else {
                UPSTREAM_COLLAPSE_CLIENT_MESSAGE
            };
            (mapped, "api_error", truncate_error_body(message))
        }
        RouteError::UpstreamTimeout(_msg) => (
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            "upstream request timed out".to_owned(),
        ),
        RouteError::ProviderDecode(_msg) => (
            StatusCode::BAD_GATEWAY,
            "api_error",
            PROVIDER_DECODE_CLIENT_MESSAGE.to_owned(),
        ),
        RouteError::Internal(_msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            INTERNAL_ERROR_CLIENT_MESSAGE.to_owned(),
        ),
        RouteError::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "rate limit exceeded".to_owned(),
        ),
        RouteError::Conflict => (
            StatusCode::CONFLICT,
            "invalid_request_error",
            "duplicate request, please retry".to_owned(),
        ),
        RouteError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found_error",
            "not found".to_owned(),
        ),
        RouteError::RequestTimeout => (
            StatusCode::REQUEST_TIMEOUT,
            "timeout_error",
            "request timed out".to_owned(),
        ),
        RouteError::PayloadTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request body too large".to_owned(),
        ),
        RouteError::MethodNotAllowed => (
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            "method not allowed".to_owned(),
        ),
        RouteError::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or invalid client auth token".to_owned(),
        ),
    }
}
fn map_upstream_status(upstream: StatusCode, auth_owner: AuthOwner) -> StatusCode {
    match upstream.as_u16() {
        400 | 413 | 429 => upstream,
        401 | 403 if auth_owner == AuthOwner::Client => upstream,
        code => {
            if matches!(code, 401 | 403 | 404) {
                tracing::warn!(
                    upstream_status = code,
                    auth_owner = ?auth_owner,
                    "upstream returned a status that may indicate a configuration \
                     issue (auth, forbidden, or not found); mapping to 502 for client"
                );
            } else if code < 400 {
                tracing::debug!(
                    upstream_status = code,
                    "unexpected 1xx/2xx/3xx upstream status in error path; mapping to 502"
                );
            }
            StatusCode::BAD_GATEWAY
        }
    }
}
const MAX_ERROR_MESSAGE_LEN: usize = 512;
const TRUNCATED_SUFFIX: &str = "...[truncated]";
pub(crate) fn truncate_with_suffix(s: &str, max_len: usize, suffix: &str) -> String {
    llm_proxy_protocol::util::truncate_with_suffix(s, max_len, suffix)
}
fn truncate_error_body(body: &str) -> String {
    truncate_with_suffix(body, MAX_ERROR_MESSAGE_LEN, TRUNCATED_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_invalid_request_returns_400() {
        let err = RouteError::InvalidRequest("bad input".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn anthropic_model_not_allowed_returns_400() {
        let err = RouteError::ModelNotAllowed("gpt-99".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn anthropic_upstream_500_returns_502() {
        let err = RouteError::Upstream {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "upstream error".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn anthropic_upstream_429_returns_429() {
        let err = RouteError::Upstream {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: "rate limited".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn anthropic_provider_decode_returns_502() {
        let err = RouteError::ProviderDecode("bad frame with sensitive data".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn anthropic_internal_returns_500() {
        let err = RouteError::Internal("config missing".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn anthropic_rate_limited_returns_429() {
        let err = RouteError::RateLimited;
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn anthropic_conflict_returns_409() {
        let err = RouteError::Conflict;
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn internal_error_message_is_sanitized_in_response_body() {
        let err = RouteError::Internal("secret config detail: /etc/proxy.toml".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, INTERNAL_ERROR_CLIENT_MESSAGE);
        assert!(
            !message.contains("secret"),
            "internal error message must not contain internal details"
        );
    }

    #[tokio::test]
    async fn provider_decode_error_message_is_sanitized_in_response_body() {
        let err = RouteError::ProviderDecode(
            "decode response: upstream returned malformed JSON with api_key=sk-ant-abc123".into(),
        );
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, PROVIDER_DECODE_CLIENT_MESSAGE);
        assert!(
            !message.contains("upstream"),
            "provider decode error must not contain upstream details"
        );
    }

    #[test]
    fn openai_invalid_request_returns_400() {
        let err = RouteError::InvalidRequest("bad input".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn openai_model_not_allowed_returns_400() {
        let err = RouteError::ModelNotAllowed("gpt-99".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn openai_upstream_500_returns_502() {
        let err = RouteError::Upstream {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "upstream error".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn openai_upstream_429_returns_429() {
        let err = RouteError::Upstream {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: "rate limited".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn openai_provider_decode_returns_502() {
        let err = RouteError::ProviderDecode("bad frame".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn openai_internal_returns_500() {
        let err = RouteError::Internal("config missing".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn openai_rate_limited_returns_429() {
        let err = RouteError::RateLimited;
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn openai_conflict_returns_409() {
        let err = RouteError::Conflict;
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn openai_invalid_request_body_has_correct_structure() {
        let err = RouteError::InvalidRequest("bad input".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert!(json["error"].is_object(), "must have error object");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "bad input");
        assert!(json["error"]["code"].is_null(), "code must be null");
        assert!(
            json.get("type").is_none() || json["type"].is_null(),
            "must not have Anthropic 'type' field"
        );
    }

    #[tokio::test]
    async fn openai_model_not_allowed_body_has_correct_structure() {
        let err = RouteError::ModelNotAllowed("gpt-99".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("gpt-99")
        );
        assert!(json["error"]["code"].is_null());
    }

    #[tokio::test]
    async fn openai_upstream_500_body_has_correct_structure() {
        let err = RouteError::Upstream {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "model 'gpt-foo' is overloaded: request echoed here".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "api_error");
        assert_eq!(json["error"]["message"], UPSTREAM_COLLAPSE_CLIENT_MESSAGE);
        assert!(json["error"]["code"].is_null());
        let message = json["error"]["message"].as_str().unwrap();
        assert!(!message.contains("gpt-foo"));
        assert!(!message.contains("overloaded"));
        assert!(!message.contains("echoed"));
    }

    #[tokio::test]
    async fn upstream_passthrough_400_uses_generic_message() {
        let err = RouteError::Upstream {
            status: StatusCode::BAD_REQUEST,
            body: r#"{"error":{"message":"model 'gpt-foo' does not exist","type":"invalid_request_error"}}"#.into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, UPSTREAM_PASSTHROUGH_CLIENT_MESSAGE);
        assert!(!message.contains("gpt-foo"));
        assert!(!message.contains("invalid_request_error"));
    }

    #[tokio::test]
    async fn upstream_passthrough_401_client_uses_generic_message_and_passes_through() {
        let err = RouteError::Upstream {
            status: StatusCode::UNAUTHORIZED,
            body: "invalid api key sk-leaked".into(),
            auth_owner: AuthOwner::Client,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, UPSTREAM_PASSTHROUGH_CLIENT_MESSAGE);
        assert!(!message.contains("sk-leaked"));
    }

    #[tokio::test]
    async fn upstream_401_operator_collapses_to_502() {
        let err = RouteError::Upstream {
            status: StatusCode::UNAUTHORIZED,
            body: "invalid api key".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn openai_internal_message_is_sanitized() {
        let err = RouteError::Internal("secret config detail: /etc/proxy.toml".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, INTERNAL_ERROR_CLIENT_MESSAGE);
        assert!(
            !message.contains("secret"),
            "internal error message must not contain internal details"
        );
    }

    #[tokio::test]
    async fn openai_provider_decode_message_is_sanitized() {
        let err = RouteError::ProviderDecode(
            "decode response: upstream returned malformed JSON with api_key=sk-ant-abc123".into(),
        );
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let message = json["error"]["message"].as_str().unwrap();
        assert_eq!(message, PROVIDER_DECODE_CLIENT_MESSAGE);
    }

    #[tokio::test]
    async fn openai_rate_limited_body_has_correct_structure() {
        let err = RouteError::RateLimited;
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "rate_limit_error");
        assert_eq!(json["error"]["message"], "rate limit exceeded");
        assert!(json["error"]["code"].is_null());
    }

    #[tokio::test]
    async fn openai_conflict_body_has_correct_structure() {
        let err = RouteError::Conflict;
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("duplicate")
        );
        assert!(json["error"]["code"].is_null());
    }

    #[test]
    fn openai_error_has_json_content_type() {
        let err = RouteError::InvalidRequest("test".into());
        let response = route_error_response(ClientProtocol::OpenAiChat, &err);
        let ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content-type header");
        assert_eq!(ct, "application/json");
    }

    #[test]
    fn request_timeout_returns_408() {
        let response = route_error_response(ClientProtocol::Anthropic, &RouteError::RequestTimeout);
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[test]
    fn payload_too_large_returns_413() {
        let response =
            route_error_response(ClientProtocol::Anthropic, &RouteError::PayloadTooLarge);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn request_timeout_anthropic_body_is_json_envelope() {
        let response = route_error_response(ClientProtocol::Anthropic, &RouteError::RequestTimeout);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "timeout_error");
        assert_eq!(json["error"]["message"], "request timed out");
    }

    #[tokio::test]
    async fn payload_too_large_openai_body_is_json_envelope() {
        let response =
            route_error_response(ClientProtocol::OpenAiChat, &RouteError::PayloadTooLarge);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(json["error"]["code"].is_null());
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too large")
        );
    }

    #[test]
    fn method_not_allowed_returns_405() {
        let response =
            route_error_response(ClientProtocol::Anthropic, &RouteError::MethodNotAllowed);
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn method_not_allowed_anthropic_body_is_json_envelope() {
        let response =
            route_error_response(ClientProtocol::Anthropic, &RouteError::MethodNotAllowed);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "method not allowed");
    }

    #[tokio::test]
    async fn method_not_allowed_openai_body_is_json_envelope() {
        let response =
            route_error_response(ClientProtocol::OpenAiChat, &RouteError::MethodNotAllowed);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "method not allowed");
        assert!(json["error"]["code"].is_null());
        assert!(
            json.get("type").is_none() || json["type"].is_null(),
            "OpenAI 405 must not carry the Anthropic 'type' field"
        );
    }

    #[test]
    fn route_error_variant_name_returns_variant_identifier() {
        assert_eq!(
            route_error_variant_name(&RouteError::UnknownProvider("p".into())),
            "UnknownProvider"
        );
        assert_eq!(route_error_variant_name(&RouteError::NotFound), "NotFound");
        assert_eq!(
            route_error_variant_name(&RouteError::RateLimited),
            "RateLimited"
        );
        assert_eq!(
            route_error_variant_name(&RouteError::RequestTimeout),
            "RequestTimeout"
        );
        assert_eq!(
            route_error_variant_name(&RouteError::PayloadTooLarge),
            "PayloadTooLarge"
        );
        assert_eq!(
            route_error_variant_name(&RouteError::MethodNotAllowed),
            "MethodNotAllowed"
        );
        assert_eq!(
            route_error_variant_name(&RouteError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: String::new(),
                auth_owner: AuthOwner::Operator,
            }),
            "Upstream"
        );
        let (status, client_type, _msg) =
            extract_error_fields(&RouteError::UnknownProvider("p".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_ne!(client_type, "UnknownProvider");
    }

    #[test]
    fn upstream_400_passes_through() {
        assert_eq!(
            map_upstream_status(StatusCode::BAD_REQUEST, AuthOwner::Operator),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn upstream_401_operator_maps_to_502() {
        assert_eq!(
            map_upstream_status(StatusCode::UNAUTHORIZED, AuthOwner::Operator),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn upstream_401_client_passes_through() {
        assert_eq!(
            map_upstream_status(StatusCode::UNAUTHORIZED, AuthOwner::Client),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn upstream_403_operator_maps_to_502() {
        assert_eq!(
            map_upstream_status(StatusCode::FORBIDDEN, AuthOwner::Operator),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn upstream_403_client_passes_through() {
        assert_eq!(
            map_upstream_status(StatusCode::FORBIDDEN, AuthOwner::Client),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn upstream_413_passes_through() {
        assert_eq!(
            map_upstream_status(StatusCode::PAYLOAD_TOO_LARGE, AuthOwner::Operator),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn upstream_429_maps_to_429() {
        assert_eq!(
            map_upstream_status(StatusCode::TOO_MANY_REQUESTS, AuthOwner::Operator),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn upstream_404_maps_to_502() {
        assert_eq!(
            map_upstream_status(StatusCode::NOT_FOUND, AuthOwner::Client),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn upstream_500_maps_to_502() {
        assert_eq!(
            map_upstream_status(StatusCode::INTERNAL_SERVER_ERROR, AuthOwner::Operator),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn truncate_short_body_unchanged() {
        let body = "short error";
        assert_eq!(truncate_error_body(body), body);
    }

    #[test]
    fn truncate_long_body() {
        let body = "x".repeat(600);
        let truncated = truncate_error_body(&body);
        assert!(truncated.ends_with("...[truncated]"));
        assert!(
            truncated.len() <= MAX_ERROR_MESSAGE_LEN,
            "truncated body must not exceed MAX_ERROR_MESSAGE_LEN, got {}",
            truncated.len()
        );
    }

    #[test]
    fn truncate_exact_length_is_unchanged() {
        let body = "a".repeat(MAX_ERROR_MESSAGE_LEN);
        assert_eq!(truncate_error_body(&body), body);
        let body = "a".repeat(MAX_ERROR_MESSAGE_LEN + 1);
        let truncated = truncate_error_body(&body);
        assert!(truncated.ends_with("...[truncated]"));
        assert!(truncated.len() <= MAX_ERROR_MESSAGE_LEN);
    }

    #[test]
    fn truncate_multibyte_at_boundary_does_not_panic() {
        let body = "あ".repeat(200);
        let truncated = truncate_error_body(&body);
        assert!(truncated.len() <= MAX_ERROR_MESSAGE_LEN);
        assert!(truncated.ends_with("...[truncated]"));
    }

    #[test]
    fn anthropic_error_has_json_content_type() {
        let err = RouteError::InvalidRequest("test".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content-type header");
        assert_eq!(ct, "application/json");
    }

    #[tokio::test]
    async fn anthropic_invalid_request_body_has_correct_structure() {
        let err = RouteError::InvalidRequest("bad input".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "bad input");
    }

    #[tokio::test]
    async fn anthropic_model_not_allowed_body_has_correct_structure() {
        let err = RouteError::ModelNotAllowed("gpt-99".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("gpt-99")
        );
    }

    #[tokio::test]
    async fn anthropic_upstream_500_body_has_correct_structure() {
        let err = RouteError::Upstream {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "model 'gpt-foo' is overloaded: request echoed here".into(),
            auth_owner: AuthOwner::Operator,
        };
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "api_error");
        assert_eq!(json["error"]["message"], UPSTREAM_COLLAPSE_CLIENT_MESSAGE);
        let message = json["error"]["message"].as_str().unwrap();
        assert!(!message.contains("gpt-foo"));
        assert!(!message.contains("overloaded"));
        assert!(!message.contains("echoed"));
    }

    #[tokio::test]
    async fn anthropic_internal_body_uses_generic_message() {
        let err = RouteError::Internal("config missing".into());
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "api_error");
        assert_eq!(json["error"]["message"], INTERNAL_ERROR_CLIENT_MESSAGE);
    }

    #[tokio::test]
    async fn anthropic_rate_limited_body_has_correct_structure() {
        let err = RouteError::RateLimited;
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "rate_limit_error");
        assert_eq!(json["error"]["message"], "rate limit exceeded");
    }

    #[tokio::test]
    async fn anthropic_conflict_body_has_correct_structure() {
        let err = RouteError::Conflict;
        let response = route_error_response(ClientProtocol::Anthropic, &err);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("duplicate")
        );
    }

    #[test]
    fn client_protocol_equality() {
        assert_eq!(ClientProtocol::Anthropic, ClientProtocol::Anthropic);
        assert_ne!(ClientProtocol::Anthropic, ClientProtocol::OpenAiChat);
    }

    #[test]
    fn stream_error_json_produces_valid_structure() {
        let json_str = openai_stream_error_json_with_type("test error", "server_error");
        assert!(json_str.is_some());
        let json: serde_json::Value = serde_json::from_str(&json_str.unwrap()).unwrap();
        assert_eq!(json["error"]["message"], "test error");
        assert_eq!(json["error"]["type"], "server_error");
        assert!(json["error"]["code"].is_null());
    }

    #[test]
    fn stream_error_json_with_empty_message() {
        let json_str = openai_stream_error_json_with_type("", "api_error");
        assert!(json_str.is_some());
        let json: serde_json::Value = serde_json::from_str(&json_str.unwrap()).unwrap();
        assert_eq!(json["error"]["message"], "");
    }
}
