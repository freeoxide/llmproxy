//! The engine's request/response surface: core types in, core types out.

use std::time::Instant;

use futures::stream::BoxStream;
use llm_proxy_adapters::client_wire::ClientWireRequest;
use llm_proxy_core::ProviderRouteKind;
use llm_proxy_protocol::core::{CoreEvent, CoreRequest, CoreResponse};
use secrecy::SecretString;

use crate::traits::InboundHeaders;

/// The client dialect a request arrived in; recorded on usage rows and used
/// by the server to pick the response codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientDialect {
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    OpenAiChat,
    /// Anthropic Messages (`/v1/messages`).
    Anthropic,
}

/// Inbound request context the pipeline needs beyond the decoded core: the
/// client's auth token (for passthrough auth), the client's headers (for the
/// forwarding allowlist), and the client's raw wire request.
#[derive(Debug, Clone)]
pub struct InboundRequest {
    /// Bearer/x-api-key token from the client, if any.
    pub auth: Option<SecretString>,
    /// The client's request headers, names lowercased, UTF-8 values only.
    pub headers: InboundHeaders,
    /// The client's raw wire request (dialect + body).
    pub wire: ClientWireRequest,
}

/// One request to run through the engine. Everything the lifecycle needs
/// beyond the core request; the server fills it from the HTTP layer, an SDK
/// caller from code.
#[derive(Debug, Clone)]
pub struct EngineRequest {
    /// Provider named in the URL (`/providers/{provider}/...`).
    pub provider_name: String,
    /// Which endpoint family the client called.
    pub route_kind: ProviderRouteKind,
    /// Which dialect the client speaks (recorded on usage rows).
    pub client_dialect: ClientDialect,
    /// The decoded, normalized request.
    pub core: CoreRequest,
    /// Auth token, headers and raw wire request as they arrived.
    pub inbound: InboundRequest,
    /// Proxy-generated request id.
    pub request_id: String,
    /// Caller identity (key id or `"anonymous"`).
    pub caller: String,
    /// SHA-256 of the raw client body (never the body itself).
    pub body_hash: String,
    /// When the request entered the pipeline.
    pub start: Instant,
}

impl EngineRequest {
    /// Start the pipeline clock now (the server overrides it with the time
    /// the HTTP request arrived).
    #[must_use]
    pub fn new(
        provider_name: impl Into<String>,
        route_kind: ProviderRouteKind,
        client_dialect: ClientDialect,
        core: CoreRequest,
        inbound: InboundRequest,
    ) -> Self {
        Self {
            provider_name: provider_name.into(),
            route_kind,
            client_dialect,
            core,
            inbound,
            request_id: format!("req-{}", uuid::Uuid::new_v4()),
            caller: llm_proxy_core::ANONYMOUS_CALLER.to_owned(),
            body_hash: String::new(),
            start: Instant::now(),
        }
    }

    /// Set the request id.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = request_id.into();
        self
    }

    /// Set the caller identity.
    #[must_use]
    pub fn with_caller(mut self, caller: impl Into<String>) -> Self {
        self.caller = caller.into();
        self
    }

    /// Set the raw-body digest.
    #[must_use]
    pub fn with_body_hash(mut self, body_hash: impl Into<String>) -> Self {
        self.body_hash = body_hash.into();
        self
    }

    /// Set the pipeline entry time.
    #[must_use]
    pub fn with_start(mut self, start: Instant) -> Self {
        self.start = start;
        self
    }
}

/// A completed non-streaming response: the decoded [`CoreResponse`] plus, on
/// same-format passthrough routes, the exact upstream body to return to the
/// client (conflict 10).
#[derive(Debug, Clone)]
pub struct EngineResponse {
    /// The decoded response.
    pub core: CoreResponse,
    /// Same-format passthrough body: the upstream bytes, verbatim.
    pub raw_body: Option<Vec<u8>>,
}

/// A streaming response: the buffered prefix that crossed the pre-commit
/// window, then the continuation. Polling the tail drives settlement; when
/// the tail is dropped, the engine records a client cancellation and settles
/// the request as failed.
pub struct EngineStream {
    /// The events decoded before the stream was committed to the caller.
    pub head: Vec<CoreEvent>,
    /// The remaining events.
    pub tail: BoxStream<'static, CoreEvent>,
}

impl std::fmt::Debug for EngineStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineStream")
            .field("head_len", &self.head.len())
            .field("tail", &"<core event stream>")
            .finish()
    }
}
