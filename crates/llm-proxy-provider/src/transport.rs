//! Protocol-neutral HTTP transport: sends prepared bytes to a prepared URL
//! with prepared auth headers, knowing nothing about providers or protocols.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use futures::TryStreamExt;
use llm_proxy_core::AuthStyle;
use secrecy::{ExposeSecret, SecretString};
use tokio::time::Sleep;

use crate::error::ProviderError;

/// Default time-to-first-byte (response headers) deadline for upstream requests;
/// bounds the headers phase without capping the streaming body.
pub const DEFAULT_UPSTREAM_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 2048;

/// Authentication headers for an upstream request; `api_key` is redacted in
/// [`fmt::Debug`] output so logs and snapshots never leak the secret.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ProxyRequest {
    /// Full upstream URL.
    pub url: String,
    /// Authentication headers.
    pub auth: AuthHeaders,
    /// Raw request body bytes.
    pub body: Vec<u8>,
    /// Informational only: NOT read by [`ProxyClient`], which the caller picks
    /// (`send` vs `send_stream`); exists for structured logging and metrics.
    pub stream: bool,
    /// Static adapter headers from config (e.g. `anthropic-version`), applied to every upstream request.
    pub extra_headers: std::collections::HashMap<String, String>,
}

impl fmt::Debug for ProxyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyRequest")
            .field("url", &endpoint_without_query(&self.url))
            .field("auth", &self.auth)
            .field("body_len", &self.body.len())
            .field("stream", &self.stream)
            .finish()
    }
}

pub(crate) fn endpoint_without_query(endpoint: &str) -> &str {
    endpoint
        .split_once('?')
        .map_or(endpoint, |(base, _query)| base)
}

/// Default per-chunk idle read timeout for streaming responses; a stream
/// silent for longer than this is a dead or wedged upstream.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Protocol-neutral HTTP client for upstream LLM providers, owning a
/// connection-pooled [`reqwest::Client`].
#[derive(Clone)]
#[must_use = "ProxyClient does nothing until send/send_stream is called"]
pub struct ProxyClient {
    http: reqwest::Client,
    stream_idle_timeout: Duration,
    upstream_headers_timeout: Duration,
}

impl fmt::Debug for ProxyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyClient")
            .field("http", &"<reqwest::Client>")
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("upstream_headers_timeout", &self.upstream_headers_timeout)
            .finish()
    }
}

impl Default for ProxyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyClient {
    /// Convenience wrapper around [`Self::try_new`] for the default
    /// (infallible-in-practice) configuration; `try_new` is the fallible form.
    #[expect(
        clippy::expect_used,
        reason = "documented infallible-in-practice constructor; try_new is the fallible alternative"
    )]
    pub fn new() -> Self {
        Self::try_new().expect("failed to build reqwest client with default configuration")
    }

    /// Creates a transport client, returning an error if the HTTP client cannot
    /// be built (custom TLS backends, proxy environment validation).
    pub fn try_new() -> Result<Self, ProviderError> {
        let http = pooled_http_builder().build()?;

        Ok(Self {
            http,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            upstream_headers_timeout: DEFAULT_UPSTREAM_HEADERS_TIMEOUT,
        })
    }

    /// Creates a client routed through a forward proxy with the same pool and
    /// timeouts as [`Self::try_new`]; `proxy_url` must be socks5h/socks5/http(s) with a host.
    pub fn try_with_egress_proxy(proxy_url: &str) -> Result<Self, ProviderError> {
        validate_egress_proxy_url(proxy_url)?;
        let proxy = reqwest::Proxy::all(proxy_url)?;
        let http = pooled_http_builder().proxy(proxy).build()?;

        Ok(Self {
            http,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            upstream_headers_timeout: DEFAULT_UPSTREAM_HEADERS_TIMEOUT,
        })
    }

    /// Override the per-chunk idle read timeout for streaming responses;
    /// the default is [`DEFAULT_STREAM_IDLE_TIMEOUT`].
    pub fn with_stream_idle_timeout(mut self, idle: Duration) -> Self {
        self.stream_idle_timeout = idle;
        self
    }

    /// Override the time-to-first-byte deadline for upstream requests; the
    /// default is [`DEFAULT_UPSTREAM_HEADERS_TIMEOUT`]. Does not cap the body.
    pub fn with_upstream_headers_timeout(mut self, deadline: Duration) -> Self {
        self.upstream_headers_timeout = deadline;
        self
    }

    fn build_request(
        &self,
        req: &ProxyRequest,
        accept_event_stream: bool,
    ) -> reqwest::RequestBuilder {
        let mut builder = self
            .http
            .post(&req.url)
            .header("Content-Type", "application/json");
        if accept_event_stream {
            builder = builder.header("Accept", "text/event-stream");
        }
        builder = apply_auth(builder, &req.auth);
        for (name, value) in &req.extra_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }

    /// Sends a non-streaming request and returns the response body bytes.
    /// Sets `Content-Type: application/json` but not `Accept: text/event-stream`.
    pub async fn send(&self, req: ProxyRequest) -> Result<Vec<u8>, ProviderError> {
        let builder = self.build_request(&req, false);

        let resp =
            tokio::time::timeout(self.upstream_headers_timeout, builder.body(req.body).send())
                .await
                .map_err(|elapsed| ProviderError::Http {
                    message: format!(
                        "upstream request timed out waiting for response headers within \
                 {}s (upstream headers timeout): {elapsed}; aborting to avoid an \
                 open-ended stall (audit transport-no-upstream-timeout)",
                        self.upstream_headers_timeout.as_secs()
                    ),
                    timeout: true,
                    redirect: false,
                })??;

        check_status(resp).await
    }

    /// Sends a streaming request; returns [`ProviderError::Api`] for status >= 400
    /// before any bytes are exposed. Dropping the returned stream aborts the request.
    pub async fn send_stream(
        &self,
        req: ProxyRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send + 'static>>,
        ProviderError,
    > {
        let builder = self.build_request(&req, true);

        let resp =
            tokio::time::timeout(self.upstream_headers_timeout, builder.body(req.body).send())
                .await
                .map_err(|elapsed| ProviderError::Http {
                    message: format!(
                        "upstream stream request timed out waiting for response headers \
                 within {}s (upstream headers timeout): {elapsed}; aborting to avoid \
                 an open-ended stall (audit transport-no-upstream-timeout)",
                        self.upstream_headers_timeout.as_secs()
                    ),
                    timeout: true,
                    redirect: false,
                })??;

        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body_text =
                crate::error::read_error_body_bounded(resp, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
            return Err(ProviderError::api(status, body_text));
        }

        let stream = resp.bytes_stream();
        let mapped = stream.map_err(ProviderError::from);
        let inner: Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>> =
            Box::pin(mapped);
        Ok(Box::pin(IdleTimeoutStream::new(
            inner,
            self.stream_idle_timeout,
        )))
    }
}

struct IdleTimeoutStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>,
    sleep: Pin<Box<Sleep>>,
    idle: Duration,
}

impl IdleTimeoutStream {
    fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>,
        idle: Duration,
    ) -> Self {
        Self {
            inner,
            sleep: Box::pin(tokio::time::sleep(idle)),
            idle,
        }
    }
}

impl Stream for IdleTimeoutStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                this.sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.idle);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(ProviderError::Http {
                    message: format!(
                        "upstream stream stalled: no data within {}s \
                             (stream idle timeout); aborting to avoid an \
                             open-ended stall (audit GAP-MED-2)",
                        this.idle.as_secs()
                    ),
                    timeout: true,
                    redirect: false,
                }))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

fn pooled_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
}

const EGRESS_PROXY_SCHEMES: [&str; 4] = ["socks5h", "socks5", "http", "https"];

fn validate_egress_proxy_url(proxy_url: &str) -> Result<(), ProviderError> {
    let url = proxy_url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        ProviderError::InvalidConfig(format!(
            "egress proxy URL has no scheme (expected socks5h://, socks5://, http://, or https://): {url:?}"
        ))
    })?;
    if !EGRESS_PROXY_SCHEMES
        .iter()
        .any(|accepted| scheme.eq_ignore_ascii_case(accepted))
    {
        return Err(ProviderError::InvalidConfig(format!(
            "egress proxy scheme must be socks5h://, socks5://, http://, or https://, got {scheme}://"
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return Err(ProviderError::InvalidConfig(format!(
            "egress proxy URL has no host: {url:?}"
        )));
    }
    Ok(())
}

fn apply_auth(mut builder: reqwest::RequestBuilder, auth: &AuthHeaders) -> reqwest::RequestBuilder {
    let api_key: &str = auth.api_key.expose_secret();
    match auth.style {
        AuthStyle::Bearer => {
            builder = builder.header("Authorization", format!("Bearer {}", api_key));
        }
        AuthStyle::XApiKey => {
            builder = builder.header("x-api-key", api_key);
        }
        AuthStyle::XGoogleApiKey => {
            builder = builder.header("x-goog-api-key", api_key);
        }
        AuthStyle::Both => {
            builder = builder.header("Authorization", format!("Bearer {}", api_key));
            builder = builder.header("x-api-key", api_key);
        }
    }
    builder
}

async fn check_status(resp: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if resp.status().as_u16() >= 400 {
        let status = resp.status().as_u16();
        let body_text =
            crate::error::read_error_body_bounded(resp, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
        return Err(ProviderError::api(status, body_text));
    }
    let body = resp.bytes().await?;
    Ok(body.to_vec())
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
        let debug_output = format!("{:?}", auth);
        assert!(
            !debug_output.contains("sk-super-secret-key-12345"),
            "Debug output must not contain the api_key: {}",
            debug_output
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain redacted marker: {}",
            debug_output
        );
    }

    #[test]
    fn proxy_request_debug_redacts_api_key() {
        let req = ProxyRequest {
            url: "https://api.example.com/v1/chat/completions?key=query-secret".to_owned(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("sk-super-secret-key-12345"),
            },
            body: br#"{"model":"gpt-5"}"#.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };
        let debug_output = format!("{:?}", req);
        assert!(
            !debug_output.contains("sk-super-secret-key-12345"),
            "Debug output must not contain the api_key: {}",
            debug_output
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain redacted marker: {}",
            debug_output
        );
        assert!(!debug_output.contains("query-secret"));
    }

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use futures::StreamExt;
    use tokio::net::TcpListener;

    async fn echo_handler(headers: HeaderMap, body: bytes::Bytes) -> impl IntoResponse {
        let mut response_parts = Vec::new();

        for (name, value) in &headers {
            response_parts.push(format!(
                "{}: {}",
                name.as_str(),
                value.to_str().unwrap_or("?")
            ));
        }

        let body_str = String::from_utf8_lossy(&body);
        response_parts.push(format!("body: {}", body_str));

        (StatusCode::OK, response_parts.join("\n"))
    }

    async fn echo_stream_handler(headers: HeaderMap, body: bytes::Bytes) -> impl IntoResponse {
        let mut events = Vec::new();

        if let Some(ct) = headers.get("content-type") {
            events.push(format!(
                "data: {{\"content-type\": \"{}\"}}\n\n",
                ct.to_str().unwrap_or("?")
            ));
        }

        if let Some(auth) = headers.get("authorization") {
            events.push(format!(
                "data: {{\"authorization\": \"{}\"}}\n\n",
                auth.to_str().unwrap_or("?")
            ));
        }

        if let Some(key) = headers.get("x-api-key") {
            events.push(format!(
                "data: {{\"x-api-key\": \"{}\"}}\n\n",
                key.to_str().unwrap_or("?")
            ));
        }

        if let Some(accept) = headers.get("accept") {
            events.push(format!(
                "data: {{\"accept\": \"{}\"}}\n\n",
                accept.to_str().unwrap_or("?")
            ));
        }

        let body_str = String::from_utf8_lossy(&body);
        events.push(format!("data: {{\"body\": \"{}\"}}\n\n", body_str));

        events.push("data: [DONE]\n\n".to_owned());

        let stream = futures::stream::iter(events);
        let body_stream = stream.map(|s| Ok::<_, std::convert::Infallible>(bytes::Bytes::from(s)));

        (
            StatusCode::OK,
            [("Content-Type", "text/event-stream")],
            Body::from_stream(body_stream),
        )
    }

    async fn error_handler(State(code): State<u16>) -> impl IntoResponse {
        let body = format!(r#"{{"error": "upstream error", "status": {}}}"#, code);
        (
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            body,
        )
    }

    async fn wait_for_ready(addr: std::net::SocketAddr) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("test server at {addr} did not become ready within 5 s");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn start_test_server(routes: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);

        tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });

        wait_for_ready(addr).await;

        base
    }

    #[tokio::test]
    async fn bearer_header_set() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("test-key-123"),
            },
            body: br#"{"hello":"world"}"#.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("authorization: Bearer test-key-123"),
            "Response must contain Bearer header: {}",
            text
        );
        assert!(
            !text.contains("x-api-key"),
            "Bearer auth style must NOT set x-api-key header: {}",
            text
        );
    }

    #[tokio::test]
    async fn x_api_key_header_set() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::XApiKey,
                api_key: SecretString::from("test-key-456"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("x-api-key: test-key-456"),
            "Response must contain x-api-key header: {}",
            text
        );
        assert!(
            !text.contains("authorization"),
            "XApiKey auth style must NOT set Authorization header: {}",
            text
        );
    }

    #[tokio::test]
    async fn both_headers_set() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Both,
                api_key: SecretString::from("test-key-789"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("authorization: Bearer test-key-789"),
            "Response must contain Bearer header: {}",
            text
        );
        assert!(
            text.contains("x-api-key: test-key-789"),
            "Response must contain x-api-key header: {}",
            text
        );
    }

    #[tokio::test]
    async fn x_google_api_key_sets_correct_header() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::XGoogleApiKey,
                api_key: SecretString::from("google-test-key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("x-goog-api-key: google-test-key"),
            "Response must contain x-goog-api-key header: {}",
            text
        );
        assert!(
            !text.contains("authorization:"),
            "XGoogleApiKey must not set Authorization header: {}",
            text
        );
        assert!(
            !text.contains("x-api-key:"),
            "XGoogleApiKey must not set x-api-key header: {}",
            text
        );
    }

    #[tokio::test]
    async fn extra_headers_are_sent_in_non_streaming_request() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let mut extra = std::collections::HashMap::new();
        extra.insert("anthropic-version".to_owned(), "2023-06-01".to_owned());
        extra.insert("x-custom".to_owned(), "custom-value".to_owned());

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: extra,
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("anthropic-version: 2023-06-01"),
            "Response must contain static adapter header: {}",
            text
        );
        assert!(
            text.contains("x-custom: custom-value"),
            "Response must contain static adapter header: {}",
            text
        );
    }

    #[tokio::test]
    async fn extra_headers_are_sent_in_streaming_request() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let mut extra = std::collections::HashMap::new();
        extra.insert("x-stream-header".to_owned(), "stream-value".to_owned());

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: br#"data: hello"#.to_vec(),
            stream: true,
            extra_headers: extra,
        };

        let result = client.send_stream(req).await;
        assert!(
            result.is_ok(),
            "streaming request with extra headers should succeed"
        );
    }

    #[tokio::test]
    async fn content_type_always_set_to_application_json() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: br#"{"hello":"world"}"#.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("content-type: application/json"),
            "Content-Type must always be application/json: {}",
            text
        );
    }

    #[tokio::test]
    async fn non_json_body_sent_byte_for_byte() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let raw_body: Vec<u8> = vec![0x00, 0x01, 0x02, 0xFF, 0xFE];
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: raw_body.clone(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let resp_bytes = resp;
        let text = String::from_utf8_lossy(&resp_bytes);
        assert!(
            text.contains("body:"),
            "Response must echo body field: {}",
            text
        );
        let body_prefix = b"body: \x00\x01\x02";
        assert!(
            resp_bytes
                .windows(body_prefix.len())
                .any(|w| w == body_prefix),
            "Response must contain exact raw bytes: {:?}",
            resp_bytes
        );
    }

    #[tokio::test]
    async fn error_status_returns_provider_error_api() {
        let app = Router::new()
            .route("/test", post(error_handler))
            .with_state(429u16);
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let err = client.send(req).await.unwrap_err();
        match err {
            ProviderError::Api { status, body } => {
                assert_eq!(status, 429);
                assert!(body.contains("upstream error"));
            }
            other => panic!("expected ProviderError::Api, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn streaming_error_status_returns_provider_error_api() {
        let app = Router::new()
            .route("/test", post(error_handler))
            .with_state(500u16);
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        let result = client.send_stream(req).await;
        assert!(result.is_err(), "expected error for 500 status");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected ProviderError::Api"),
        };
        match err {
            ProviderError::Api { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("upstream error"));
            }
            other => panic!("expected ProviderError::Api, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn stream_request_sets_accept_event_stream() {
        let app = Router::new().route("/test", post(echo_stream_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        let mut stream = client.send_stream(req).await.unwrap();

        let mut found_accept = false;
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            let text = String::from_utf8_lossy(&chunk);
            if text.contains("text/event-stream") {
                found_accept = true;
            }
        }
        assert!(
            found_accept,
            "Stream response must show Accept: text/event-stream was set"
        );
    }

    #[tokio::test]
    async fn non_stream_request_does_not_set_accept_event_stream() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            !text.contains("text/event-stream"),
            "Non-stream request must NOT set Accept: text/event-stream: {}",
            text
        );
    }

    #[tokio::test]
    async fn request_body_sent_byte_for_byte() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let original_body = br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}"#;
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: original_body.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        let expected = std::str::from_utf8(original_body).unwrap();
        assert!(
            text.contains(expected),
            "Response must echo the exact request body: {}",
            text
        );
    }

    async fn slow_handler(
        axum::extract::State(counter): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    ) -> axum::response::Response {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let stream = futures::stream::unfold(0u32, move |i| {
            let counter = Arc::clone(&counter);
            async move {
                if i >= 100 {
                    return None;
                }
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                let chunk = format!("data: chunk {i}\n\n");
                Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(chunk)),
                    i + 1,
                ))
            }
        });
        (
            StatusCode::OK,
            [("Content-Type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn dropping_stream_aborts_upstream() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let app = Router::new()
            .route("/test", post(slow_handler))
            .with_state(counter_clone);
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        {
            let mut stream = client.send_stream(req).await.unwrap();
            let _first = stream.next().await;
        }

        let mut count = counter.load(Ordering::SeqCst);
        let poll_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let next = counter.load(Ordering::SeqCst);
            if next == count {
                break;
            }
            count = next;
            if tokio::time::Instant::now() >= poll_deadline {
                break;
            }
        }

        assert!(
            count < 100,
            "Dropping the stream should abort upstream before it runs to \
             completion; counter was {}",
            count
        );
    }

    async fn stalling_handler() -> axum::response::Response {
        let stream = futures::stream::unfold(false, |sent| async move {
            if !sent {
                Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from("data: first\n\n")),
                    true,
                ))
            } else {
                tokio::time::sleep(Duration::from_secs(120)).await;
                None
            }
        });
        (
            StatusCode::OK,
            [("Content-Type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn stalled_upstream_triggers_idle_timeout() {
        let app = Router::new().route("/test", post(stalling_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new().with_stream_idle_timeout(Duration::from_millis(100));
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        let mut stream = client.send_stream(req).await.unwrap();

        let first = stream.next().await.expect("first chunk should arrive");
        assert!(first.is_ok(), "first chunk should be Ok: {:?}", first);

        let second = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        match second {
            Ok(Some(Err(e))) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("stalled") || msg.contains("idle"),
                    "expected idle-timeout error, got: {msg}"
                );
            }
            Ok(Some(Ok(_))) => panic!("expected idle-timeout error, got another data chunk"),
            Ok(None) => panic!("expected idle-timeout error, stream ended cleanly instead"),
            Err(_) => panic!("idle timeout did not fire within 5s"),
        }
    }

    async fn partial_disconnect_handler() -> axum::response::Response {
        let stream = futures::stream::unfold(0u32, |i| async move {
            if i >= 2 {
                None
            } else {
                let chunk = format!("data: chunk {}\n\n", i);
                Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(chunk)),
                    i + 1,
                ))
            }
        });
        (
            StatusCode::OK,
            [("Content-Type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn upstream_disconnect_ends_stream_gracefully() {
        let app = Router::new().route("/test", post(partial_disconnect_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        let mut stream = client.send_stream(req).await.unwrap();
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => chunks.push(bytes),
                Err(e) => {
                    panic!("unexpected mid-stream error: {:?}", e);
                }
            }
        }
        assert!(
            !chunks.is_empty(),
            "Expected at least one chunk before upstream disconnect"
        );
        let combined = chunks.concat();
        let all_text = String::from_utf8_lossy(&combined);
        assert!(all_text.contains("chunk 0"), "Should have received chunk 0");
        assert!(all_text.contains("chunk 1"), "Should have received chunk 1");
    }

    async fn mid_stream_error_handler() -> axum::response::Response {
        let stream = futures::stream::iter(vec![
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from("data: first\n\n")),
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from("data: second\n\n")),
        ]);
        (
            StatusCode::OK,
            [("Content-Type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn stream_collects_all_chunks_then_ends() {
        let app = Router::new().route("/test", post(mid_stream_error_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: true,
            extra_headers: std::collections::HashMap::new(),
        };

        let mut stream = client.send_stream(req).await.unwrap();
        let mut all_data = Vec::new();
        while let Some(item) = stream.next().await {
            let bytes = item.expect("chunk should be Ok");
            all_data.push(bytes);
        }
        assert_eq!(all_data.len(), 2, "Expected exactly 2 chunks");
        assert!(String::from_utf8_lossy(&all_data[0]).contains("first"));
        assert!(String::from_utf8_lossy(&all_data[1]).contains("second"));
    }

    #[tokio::test]
    async fn empty_url_returns_http_error() {
        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: String::new(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let err = client.send(req).await.unwrap_err();
        match err {
            ProviderError::Http { .. } => {}
            other => panic!(
                "expected ProviderError::Http for empty URL, got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn large_body_sent_successfully() {
        let app = axum::Router::new()
            .route("/test", post(echo_handler))
            .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let large_body = "x".repeat(5 * 1024 * 1024);
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: large_body.clone().into_bytes(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains(&large_body),
            "Response must echo the full large body back"
        );
    }

    fn prod_source() -> &'static str {
        let source = include_str!("transport.rs");
        source
            .split_once("#[cfg(test)]")
            .map(|(prod, _)| prod)
            .unwrap_or(source)
    }

    #[test]
    fn transport_source_no_endpoint_classification() {
        let source = prod_source();
        assert!(
            !source.contains("EndpointType"),
            "transport.rs must not mention EndpointType"
        );
        assert!(
            !source.contains("classify_endpoint"),
            "transport.rs must not mention classify_endpoint"
        );
    }

    #[test]
    fn transport_source_no_provider_model() {
        let source = prod_source();
        assert!(
            !source.contains("OpenCodeClient"),
            "transport.rs must not mention OpenCodeClient"
        );
        assert!(
            !source.contains("opencode_go"),
            "transport.rs must not mention provider names"
        );
        assert!(
            !source.contains("opencode_zen"),
            "transport.rs must not mention provider names"
        );
        assert!(
            !source.contains("is_anthropic_model"),
            "transport.rs must not mention model classification"
        );
        assert!(
            !source.contains("is_gemini_model"),
            "transport.rs must not mention model classification"
        );
        assert!(
            !source.contains("is_responses_model"),
            "transport.rs must not mention model classification"
        );
    }

    #[test]
    fn transport_source_no_protocol_import() {
        let source = prod_source();
        assert!(
            !source.contains("llm_proxy_protocol"),
            "transport.rs must not import llm_proxy_protocol"
        );
    }

    #[test]
    fn egress_proxy_client_builds_for_each_supported_scheme() {
        for url in [
            "socks5h://tunnel.example.com:1080",
            "socks5://127.0.0.1:1080",
            "http://proxy.internal:3128",
            "https://proxy.internal:8443",
            "SOCKS5H://tunnel.example.com:1080",
            "http://user:pass@proxy.internal:3128",
            "socks5h://tunnel.example.com",
        ] {
            let client = ProxyClient::try_with_egress_proxy(url)
                .unwrap_or_else(|e| panic!("egress proxy {url} must build: {e}"));
            assert_eq!(
                client.stream_idle_timeout, DEFAULT_STREAM_IDLE_TIMEOUT,
                "egress client must keep the default stream idle timeout"
            );
            assert_eq!(
                client.upstream_headers_timeout, DEFAULT_UPSTREAM_HEADERS_TIMEOUT,
                "egress client must keep the default headers timeout"
            );
        }
    }

    #[test]
    fn egress_proxy_client_rejects_unsupported_or_malformed_urls() {
        for url in [
            "ftp://proxy:21",
            "socks4://127.0.0.1:1080",
            "127.0.0.1:1080",
            "socks5h://",
            "",
        ] {
            let err = ProxyClient::try_with_egress_proxy(url)
                .expect_err(&format!("egress proxy {url:?} must be rejected"));
            assert!(
                matches!(err, ProviderError::InvalidConfig(_)),
                "egress proxy {url:?} must be InvalidConfig, got {err:?}"
            );
        }
    }

    async fn absolute_target_echo_handler(uri: axum::http::Uri) -> String {
        uri.to_string()
    }

    #[test]
    fn egress_proxy_client_debug_does_not_print_the_proxy() {
        let client = ProxyClient::try_with_egress_proxy("socks5h://debug-secret-host.invalid:1080")
            .expect("builds");
        let debug = format!("{client:?}");
        assert!(
            !debug.contains("debug-secret-host.invalid"),
            "ProxyClient Debug must not print the egress proxy: {debug}"
        );
    }

    #[tokio::test]
    async fn egress_proxy_client_sends_requests_through_the_proxy() {
        let app = Router::new().route("/v1/test", post(absolute_target_echo_handler));
        let proxy_base = start_test_server(app).await;

        let client =
            ProxyClient::try_with_egress_proxy(&proxy_base).expect("http egress proxy builds");
        let req = ProxyRequest {
            url: "http://egress-upstream.invalid/v1/test".to_owned(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: br#"{"hello":"world"}"#.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.expect("request via proxy succeeds");
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.contains("http://egress-upstream.invalid/v1/test"),
            "the proxy must receive the absolute-form upstream target, got: {text}"
        );
    }

    async fn read_exact(stream: &mut tokio::net::TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
        use tokio::io::AsyncReadExt;
        let mut filled = 0;
        while filled < buf.len() {
            let n = stream.read(&mut buf[filled..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "socks test client closed early",
                ));
            }
            filled += n;
        }
        Ok(())
    }

    async fn serve_one_socks5_connect(
        listener: tokio::net::TcpListener,
        upstream: std::net::SocketAddr,
        destination_tx: tokio::sync::oneshot::Sender<String>,
    ) {
        use tokio::io::AsyncWriteExt;
        let (mut sock, _peer) = listener.accept().await.expect("socks client connects");

        let mut head = [0u8; 2];
        read_exact(&mut sock, &mut head).await.expect("greeting");
        let mut methods = vec![0u8; head[1] as usize];
        read_exact(&mut sock, &mut methods).await.expect("methods");
        sock.write_all(&[0x05, 0x00]).await.expect("auth reply");

        let mut req_head = [0u8; 4];
        read_exact(&mut sock, &mut req_head)
            .await
            .expect("request head");
        assert_eq!(req_head[1], 0x01, "expected CONNECT");
        let addr_len = match req_head[3] {
            0x01 => 4,
            0x04 => 16,
            0x03 => {
                let mut len = [0u8; 1];
                read_exact(&mut sock, &mut len)
                    .await
                    .expect("domain length");
                len[0] as usize
            }
            other => panic!("unknown SOCKS address type {other}"),
        };
        let mut addr_bytes = vec![0u8; addr_len];
        read_exact(&mut sock, &mut addr_bytes)
            .await
            .expect("address");
        let mut port = [0u8; 2];
        read_exact(&mut sock, &mut port).await.expect("port");
        let destination = if req_head[3] == 0x03 {
            String::from_utf8(addr_bytes).expect("domain is UTF-8")
        } else {
            format!("{}-byte address", addr_bytes.len())
        };

        let up_port = upstream.port().to_be_bytes();
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, up_port[0], up_port[1]])
            .await
            .expect("connect reply");

        destination_tx
            .send(destination)
            .expect("test holds the receiver");

        let up = tokio::net::TcpStream::connect(upstream)
            .await
            .expect("connect to upstream");
        let (mut client_read, mut client_write) = sock.into_split();
        let (mut up_read, mut up_write) = up.into_split();
        let a = tokio::spawn(async move { tokio::io::copy(&mut client_read, &mut up_write).await });
        let b = tokio::spawn(async move { tokio::io::copy(&mut up_read, &mut client_write).await });
        let (_, _) = tokio::join!(a, b);
    }

    #[tokio::test]
    async fn socks5h_egress_delivers_the_hostname_to_the_proxy_tunnel() {
        let app = Router::new().route("/socks-test", post(echo_handler));
        let upstream_base = start_test_server(app).await;
        let upstream_addr: std::net::SocketAddr = upstream_base
            .trim_start_matches("http://")
            .parse()
            .expect("upstream addr");

        let socks_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks_listener.local_addr().unwrap();
        let (destination_tx, destination_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(serve_one_socks5_connect(
            socks_listener,
            upstream_addr,
            destination_tx,
        ));

        let client = ProxyClient::try_with_egress_proxy(&format!("socks5h://{socks_addr}"))
            .expect("socks5h egress proxy builds");
        let req = ProxyRequest {
            url: "http://egress-upstream.invalid/socks-test".to_owned(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: br#"{"via":"socks"}"#.to_vec(),
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.expect("request via socks tunnel");
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.contains(r#""via":"socks""#),
            "the echo upstream must see the body through the tunnel, got: {text}"
        );

        let destination = destination_rx.await.expect("destination");
        assert_eq!(
            destination, "egress-upstream.invalid",
            "socks5h must deliver the unresolved hostname to the proxy"
        );
    }

    #[tokio::test]
    async fn empty_body_sent_successfully() {
        let app = Router::new().route("/test", post(echo_handler));
        let base = start_test_server(app).await;

        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: format!("{}/test", base),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let resp = client.send(req).await.unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.contains("body: "),
            "Response must echo body field even when empty: {}",
            text
        );
    }

    #[tokio::test]
    async fn unreachable_url_returns_http_error() {
        let client = ProxyClient::new();
        let req = ProxyRequest {
            url: "http://192.0.2.1:1/test".to_owned(),
            auth: AuthHeaders {
                style: AuthStyle::Bearer,
                api_key: SecretString::from("key"),
            },
            body: vec![],
            stream: false,
            extra_headers: std::collections::HashMap::new(),
        };

        let result = tokio::time::timeout(Duration::from_secs(15), client.send(req)).await;
        let err = match result {
            Ok(Err(e)) => e,
            Ok(Ok(_)) => panic!("expected error for unreachable URL, got success"),
            Err(_) => panic!("test timed out waiting for connection failure"),
        };
        match err {
            ProviderError::Http { .. } => {}
            other => panic!(
                "expected ProviderError::Http for unreachable URL, got: {:?}",
                other
            ),
        }
    }
}
