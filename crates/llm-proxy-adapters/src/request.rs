//! The codec-output side of the transport boundary: a fully encoded upstream
//! request as plain bytes, URL, and headers. No networking lives here — the
//! `llm-proxy-transport` crate sends these.

use std::collections::HashMap;
use std::fmt;

use llm_proxy_core::AuthStyle;
use secrecy::SecretString;

/// Authentication headers for an upstream request; `api_key` is redacted in
/// [`fmt::Debug`] output so logs and snapshots never leak the secret.
#[derive(Clone, Debug)]
pub struct AuthHeaders {
    /// Which header style to use for the API key.
    pub style: AuthStyle,
    /// The API key value, wrapped in [`SecretString`] (redacted in Debug,
    /// zeroized on drop).
    pub api_key: SecretString,
}

/// A protocol-neutral request to an upstream provider; the transport sends
/// `body` byte-for-byte and does not serialize typed request structs.
#[derive(Clone)]
pub struct ProxyRequest {
    /// Full upstream URL.
    pub url: String,
    /// Authentication headers.
    pub auth: AuthHeaders,
    /// Raw request body bytes.
    pub body: Vec<u8>,
    /// Informational only: NOT read by the transport's `ProxyClient`, which
    /// the caller picks (`send` vs `send_stream`); exists for structured
    /// logging and metrics.
    pub stream: bool,
    /// Static adapter headers from config (e.g. `anthropic-version`), applied to every upstream request.
    pub extra_headers: HashMap<String, String>,
    /// Inbound client headers the per-provider allowlist selected for
    /// forwarding; already filtered, so the transport applies them verbatim.
    /// Applied before `extra_headers`, so a static config header with the same
    /// name wins.
    pub forwarded_headers: HashMap<String, String>,
}

impl fmt::Debug for ProxyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyRequest")
            .field("url", &endpoint_without_query(&self.url))
            .field("auth", &self.auth)
            .field("body_len", &self.body.len())
            .field("stream", &self.stream)
            .field(
                "forwarded_header_names",
                &self.forwarded_headers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The endpoint with any query string removed, so query credentials never
/// reach logs.
#[must_use]
pub fn endpoint_without_query(endpoint: &str) -> &str {
    endpoint
        .split_once('?')
        .map_or(endpoint, |(base, _query)| base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_debug_redacts_api_key() {
        let auth = AuthHeaders {
            style: AuthStyle::Bearer,
            api_key: SecretString::from("sk-super-secret-key-12345"),
        };
        let debug_output = format!("{auth:?}");
        assert!(
            !debug_output.contains("sk-super-secret-key-12345"),
            "Debug output must not contain the api_key: {}",
            debug_output
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain redaction marker: {}",
            debug_output
        );
    }

    #[test]
    fn proxy_request_debug_redacts_api_key_and_query() {
        let req = ProxyRequest {
            url: "https://api.example.com/v1/chat/completions?key=query-secret".to_owned(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("sk-super-secret-key-12345"),
            },
            body: br#"{"model":"gpt-5"}"#.to_vec(),
            stream: false,
            extra_headers: HashMap::new(),
            forwarded_headers: HashMap::new(),
        };
        let debug_output = format!("{req:?}");
        assert!(
            !debug_output.contains("sk-super-secret-key-12345")
                && !debug_output.contains("query-secret"),
            "Debug output must redact the api key and query: {debug_output}"
        );
    }
}
