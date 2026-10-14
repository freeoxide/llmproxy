//! Provider error types shared across transport, adapters, and routes.

/// Errors produced by provider transport and adapter operations; the enum is
/// non-exhaustive, so downstream match arms must include a catch-all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// Failed to serialize or deserialize JSON.
    #[error("failed to marshal request: {0}")]
    Serialize(#[from] serde_json::Error),

    /// The HTTP request failed at the transport level; the message is stored
    /// without the request URL so query credentials cannot leak through logs.
    #[error("request failed: {message}")]
    Http {
        /// URL-free transport error text.
        message: String,
        /// Whether reqwest classified the failure as a timeout.
        timeout: bool,
        /// Whether reqwest classified the failure as a redirect-policy violation;
        /// deterministic, so [`DiscoveryClient::is_transient`] must not retry it.
        redirect: bool,
    },

    /// The upstream API returned an error status code; `body` is truncated and
    /// stripped of API-key patterns so `Display` output never leaks key material.
    #[error("API error {status}: {body}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Response body text (truncated and sanitized).
        body: String,
    },

    /// Invalid UTF-8 encountered in streamed bytes.
    #[error("invalid UTF-8 in stream: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// SSE framing error (malformed frame structure, unexpected stream
    /// termination), constructed by [`SseFramer`](crate::sse::SseFramer).
    #[error("SSE framing error: {0}")]
    SseFraming(String),

    /// The upstream provider returned an empty response (no choices, no candidates).
    #[error("empty response from upstream: {0}")]
    EmptyResponse(String),

    /// Invalid configuration or input (e.g. unsafe model name for URL interpolation).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

impl From<reqwest::Error> for ProviderError {
    fn from(error: reqwest::Error) -> Self {
        let timeout = error.is_timeout();
        let redirect = error.is_redirect();
        Self::Http {
            message: error.without_url().to_string(),
            timeout,
            redirect,
        }
    }
}

pub(crate) async fn read_error_body_bounded(resp: reqwest::Response, cap: usize) -> String {
    use futures::TryStreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(2048));
    loop {
        match stream.try_next().await {
            Ok(Some(bytes)) => {
                let remaining = cap.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                let take = bytes.len().min(remaining);
                buf.extend_from_slice(&bytes[..take]);
                if buf.len() >= cap {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => return format!("<failed to read error body: {error}>"),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub(crate) const MAX_API_ERROR_BODY_LEN: usize = 512;

const TRUNCATED_SUFFIX: &str = "...[truncated]";

fn truncate_with_suffix(s: &str, max_len: usize, suffix: &str) -> String {
    llm_proxy_protocol::util::truncate_with_suffix(s, max_len, suffix)
}

static REDACTION_PATTERNS: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| {
        const C: &str = r"A-Za-z0-9_.\-/+=";
        [
            format!(r"sk-proj-[{C}]{{10,}}"),
            format!(r"sk-ant-api03-[{C}]{{10,}}"),
            format!(r"sk-ant-[{C}]{{10,}}"),
            format!(r"sk-live-[{C}]{{10,}}"),
            format!(r"sk-test-[{C}]{{10,}}"),
            format!(r"sk_live_[{C}]{{10,}}"),
            format!(r"sk_test_[{C}]{{10,}}"),
            format!(r"sk-[{C}]{{20,}}"),
            format!(r"AIza[{C}]{{30,}}"),
            format!(r"key-[{C}]{{20,}}"),
            format!(r"Bearer eyJ[{C}]{{10,}}"),
            format!(r"Bearer [{C}]{{20,}}"),
            format!(r"(?i)key=[{C}]{{20,}}"),
            format!(r"(?i)token=[{C}]{{20,}}"),
        ]
        .iter()
        .map(|pat| regex::Regex::new(pat).expect("invalid redaction regex"))
        .collect()
    });

pub(crate) fn sanitize_api_error_body(mut body: String) -> String {
    for re in REDACTION_PATTERNS.iter() {
        body = re.replace_all(&body, "***").into_owned();
    }

    if body.len() > MAX_API_ERROR_BODY_LEN {
        body = truncate_with_suffix(&body, MAX_API_ERROR_BODY_LEN, TRUNCATED_SUFFIX);
    }
    body
}

impl ProviderError {
    /// Return whether this is an HTTP timeout.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Http { timeout: true, .. })
    }

    /// Construct a [`ProviderError::Api`] with automatic body sanitization.
    pub fn api(status: u16, body_text: String) -> Self {
        Self::Api {
            status,
            body: sanitize_api_error_body(body_text),
        }
    }

    /// Classify the HTTP status of a [`ProviderError::Api`] into a
    /// [`llm_proxy_protocol::core::CoreStreamErrorKind`]; `None` for non-`Api`.
    pub fn api_error_kind(&self) -> Option<llm_proxy_protocol::core::CoreStreamErrorKind> {
        match self {
            Self::Api { status, .. } => Some(match *status {
                400 => llm_proxy_protocol::core::CoreStreamErrorKind::InvalidRequest,
                401 => llm_proxy_protocol::core::CoreStreamErrorKind::Authentication,
                403 => llm_proxy_protocol::core::CoreStreamErrorKind::Permission,
                429 => llm_proxy_protocol::core::CoreStreamErrorKind::RateLimit,
                500 | 502 | 503 => llm_proxy_protocol::core::CoreStreamErrorKind::Upstream,
                _ => llm_proxy_protocol::core::CoreStreamErrorKind::Upstream,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_body_under_limit_is_unchanged() {
        let body = "simple error message".to_owned();
        assert_eq!(sanitize_api_error_body(body.clone()), body);
    }

    #[test]
    fn sanitize_body_over_limit_is_truncated() {
        let body = "x".repeat(600);
        let result = sanitize_api_error_body(body);
        assert!(result.ends_with("...[truncated]"));
        assert!(result.len() <= MAX_API_ERROR_BODY_LEN);
    }

    #[test]
    fn sanitize_body_truncation_exact_length() {
        let body = "a".repeat(MAX_API_ERROR_BODY_LEN);
        let result = sanitize_api_error_body(body.clone());
        assert_eq!(result, body);

        let body = "a".repeat(MAX_API_ERROR_BODY_LEN + 1);
        let result = sanitize_api_error_body(body);
        assert!(result.ends_with("...[truncated]"));
        assert!(result.len() <= MAX_API_ERROR_BODY_LEN);
    }

    #[test]
    fn sanitize_body_multibyte_char_at_boundary_does_not_panic() {
        let body = "あ".repeat(200);
        let result = sanitize_api_error_body(body);
        assert!(result.len() <= MAX_API_ERROR_BODY_LEN);
        assert!(result.ends_with("...[truncated]"));
    }

    #[test]
    fn sanitize_redacts_openai_sk_live_underscore() {
        let body = r#"error: key=sk_live_abc123def456ghi789jkl012mno"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk_live_"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_openai_sk_test_underscore() {
        let body = r#"error: key=sk_test_abc123def456ghi789jkl012mno"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk_test_"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_openai_sk_live_hyphen() {
        let body = r#"error: key=sk-live-abc123def456ghi789jkl012mno"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk-live-"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_openai_sk_test_hyphen() {
        let body = r#"error: key=sk-test-abc123def456ghi789jkl012mno"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk-test-"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_anthropic_sk_ant_api03() {
        let body = r#"error: key=sk-ant-api03-abc123def456ghi789jkl012"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk-ant-api03-"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_anthropic_sk_ant() {
        let body = r#"error: key=sk-ant-abc123def456ghi789jkl012mno345"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("sk-ant-"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_google_aiza() {
        let body = r#"error: key=AIzaSyD-abc123def456ghi789jkl012mno345pqr"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("AIzaSyD-abc"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_generic_key_prefix() {
        let body = r#"error: key=key-abc123def456ghi789jkl012mno345pqr"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(!result.contains("key-abc123def456ghi789"));
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_does_not_redact_short_sk_substring() {
        let body = "the desk-area is reserved".to_owned();
        let result = sanitize_api_error_body(body.clone());
        assert_eq!(result, body);
    }

    #[test]
    fn sanitize_does_not_redact_short_key_substring() {
        let body = "key-value store".to_owned();
        let result = sanitize_api_error_body(body.clone());
        assert_eq!(result, body);
    }

    #[test]
    fn sanitize_does_not_redact_short_aiza_substring() {
        let body = "The area is at AIza Lane".to_owned();
        let result = sanitize_api_error_body(body.clone());
        assert_eq!(result, body);
    }

    #[test]
    fn sanitize_redacts_bearer_token_in_error_body() {
        let body = r#"error: invalid key Bearer sk-ant-api03-abc123def456ghi789jkl012"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("sk-ant-api03-abc123def456ghi789"),
            "Bearer token must be redacted: {}",
            result
        );
        assert!(
            result.contains("***"),
            "Must contain redaction marker: {}",
            result
        );
    }

    #[test]
    fn sanitize_redacts_generic_key_assignment() {
        let body = r#"error: key=sk_live_abc123def456ghi789jkl012mno345"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("sk_live_abc123def456ghi789"),
            "key= assignment must be redacted: {}",
            result
        );
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_generic_token_assignment() {
        let body = r#"error: token=abc123def456ghi789jkl012mno345pqr"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("abc123def456ghi789jkl012"),
            "token= assignment must be redacted: {}",
            result
        );
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_key_assignment_case_insensitive() {
        let body = r#"error: KEY=abc123def456ghi789jkl012mno345pqr"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("abc123def456ghi789jkl012"),
            "KEY= (uppercase) must be redacted: {}",
            result
        );
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_does_not_redact_short_key_assignment() {
        let body = "error: key=shortvalue".to_owned();
        let result = sanitize_api_error_body(body.clone());
        assert_eq!(result, body);
    }

    #[test]
    fn redaction_patterns_all_compile_successfully() {
        let _ = &*REDACTION_PATTERNS;
    }

    #[test]
    fn sanitize_redacts_dotted_openai_proj_key_with_no_suffix_leak() {
        let body = "invalid key: sk-proj-AbCdEfGh1234567890.T3BlbkFJabc123def456".to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("T3BlbkFJ") && !result.contains("AbCdEfGh"),
            "dotted key suffix must not leak: {result}"
        );
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_redacts_jwt_bearer_token_with_no_suffix_leak() {
        let jwt = concat!(
            "Bearer eyJhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        );
        let result = sanitize_api_error_body(jwt.to_owned());
        assert_eq!(
            result, "***",
            "the entire JWT (payload + signature) must redact, got: {result}"
        );
    }

    #[test]
    fn sanitize_redacts_legacy_dotted_key_with_no_suffix_leak() {
        let body = "error: key=abCdEf.1234567890abcdefghij.KLuMnO".to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("KLuMnO") && !result.contains("1234567890abcdefghij"),
            "dotted key= value must redact whole: {result}"
        );
        assert!(result.contains("***"));
    }

    #[test]
    fn sanitize_does_not_consume_trailing_delimiter_after_token() {
        let body = r#"{"error":"key sk-ant-abc123def456ghi789jkl012mno345pqr"}"#.to_owned();
        let result = sanitize_api_error_body(body);
        assert!(
            !result.contains("abc123def456ghi789"),
            "token body must be redacted: {result}"
        );
        assert!(
            result.contains("\"}"),
            "trailing quote/brace must be preserved, got: {result}"
        );
    }

    #[tokio::test]
    async fn http_error_does_not_expose_request_url_or_query_secret() {
        let secret = "query-secret-that-must-not-leak";
        let error = reqwest::Client::new()
            .get(format!("http://127.0.0.1:0/models?key={secret}"))
            .send()
            .await
            .expect_err("port zero must fail");
        let rendered = ProviderError::from(error).to_string();
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("127.0.0.1"));
        assert!(!rendered.contains("models?key="));
    }
}
