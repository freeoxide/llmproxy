//! Shared core pipeline for route handlers.
//!
//! Provides [`prepare_request`] (rate-limit, dedup, request-ID generation),
//! [`handle_core_once`] (non-streaming), and [`handle_core_stream`] (streaming)
//! used by `/providers/{provider}/v1/messages` and
//! `/providers/{provider}/v1/chat/completions`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use llm_proxy_core::{Metrics, ModelPricing, ProviderRouteKind, ProviderRouteResolutionError};
use llm_proxy_protocol::client::anthropic;
use llm_proxy_protocol::client::anthropic::StreamEncoder as AnthropicStreamEncoder;
use llm_proxy_protocol::client::openai_chat;
use llm_proxy_protocol::client::openai_chat::StreamEncoder as OpenAiStreamEncoder;
use llm_proxy_protocol::core::{
    CoreEvent, CoreRequest, Cost, ModelRef, StopReason, Usage, UsageProvenance,
};
use llm_proxy_provider::adapter::{
    ProviderAdapter, ProviderAdapterTarget, ProviderProtocol, ProviderStreamDecoder,
    ProviderStreamDecoderKind,
};
use llm_proxy_provider::sse::SseFramer;
use llm_proxy_provider::transport::ProxyRequest;
use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed};
use rust_decimal::Decimal;
use secrecy::SecretString;
use tracing::{Instrument, warn};

use crate::middleware::get_client_ip;
use crate::state::AppState;

use super::error_response::{
    AuthOwner, ClientProtocol, PROVIDER_DECODE_CLIENT_MESSAGE, RouteError, extract_error_fields,
    openai_stream_error_json_with_type, route_error_variant_name, truncate_with_suffix,
};

// ---------------------------------------------------------------------------
// ClientStreamEncoder — protocol-agnostic stream encoder wrapper
// ---------------------------------------------------------------------------

/// Protocol-agnostic wrapper around the Anthropic and OpenAI stream encoders.
///
/// Both encoders share the same interface (`encode_event`, `finish`,
/// `mark_finished`) but produce different output types. This enum dispatches
/// to the correct encoder based on the client protocol.
enum ClientStreamEncoder {
    Anthropic(AnthropicStreamEncoder),
    OpenAi(OpenAiStreamEncoder),
}

/// Output produced by a client stream encoder. Each variant carries the data
/// needed to construct an SSE `Event` for that protocol.
enum ClientEncodedEvent {
    /// Anthropic SSE: requires `event:` line (e.g. `event: message_start`) and
    /// JSON data. The `event_type` is the SSE event name.
    Anthropic { event_type: String, json: String },
    /// OpenAI SSE: just `data: <json>`. No event type field. The caller must
    /// also emit `data: [DONE]` as the terminal frame after the stream ends.
    OpenAi { json: String },
}

impl ClientStreamEncoder {
    /// Create a new encoder for the given protocol.
    ///
    /// For Anthropic, `msg_id` and `model` are used directly.
    /// For OpenAI, `include_usage` is extracted from `provider_hints` if present.
    fn new(protocol: ClientProtocol, msg_id: String, model: String, core: &CoreRequest) -> Self {
        match protocol {
            ClientProtocol::Anthropic => {
                Self::Anthropic(AnthropicStreamEncoder::new(msg_id, model))
            }
            ClientProtocol::OpenAiChat => {
                let include_usage = core
                    .provider_hints
                    .raw
                    .get("stream_options")
                    .and_then(|v| v.get("include_usage"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // The u64 -> i64 cast is safe: u64::MAX corresponds to a date
                // ~584 billion years in the future, unreachable in any real timeline.
                // The unwrap_or_default() handles the unlikely case of a system
                // clock set before UNIX_EPOCH (e.g. clock skew after boot) by
                // falling back to 0, which produces a valid but incorrect timestamp.
                // This is acceptable because `created` is informational only.
                let created = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                Self::OpenAi(OpenAiStreamEncoder::new(
                    msg_id,
                    model,
                    created,
                    include_usage,
                ))
            }
        }
    }

    /// Encode a single [`CoreEvent`] into zero or more protocol-specific events.
    fn encode_event(
        &mut self,
        event: CoreEvent,
    ) -> Result<Vec<ClientEncodedEvent>, llm_proxy_protocol::client::ProtocolError> {
        match self {
            Self::Anthropic(enc) => {
                let msg_events = enc.encode_event(event)?;
                Ok(wrap_anthropic_events(msg_events, "encode"))
            }
            Self::OpenAi(enc) => {
                let chunks = enc.encode_event(event)?;
                Ok(wrap_openai_events(chunks, "encode"))
            }
        }
    }

    /// Flush any remaining buffered events (synthetic terminal if needed).
    fn finish(
        &mut self,
    ) -> Result<Vec<ClientEncodedEvent>, llm_proxy_protocol::client::ProtocolError> {
        match self {
            Self::Anthropic(enc) => {
                let msg_events = enc.finish()?;
                Ok(wrap_anthropic_events(msg_events, "finish"))
            }
            Self::OpenAi(enc) => {
                let chunks = enc.finish()?;
                Ok(wrap_openai_events(chunks, "finish"))
            }
        }
    }

    /// Mark the encoder as finished so subsequent `finish()` returns empty.
    fn mark_finished(&mut self) {
        match self {
            Self::Anthropic(enc) => enc.mark_finished(),
            Self::OpenAi(enc) => enc.mark_finished(),
        }
    }
}

/// Serialize Anthropic SSE events, dropping any that fail to serialize.
///
/// `phase` is used in log messages (e.g. "encode" vs "finish") for context.
fn wrap_anthropic_events(
    events: Vec<llm_proxy_protocol::anthropic::MessageEvent>,
    phase: &str,
) -> Vec<ClientEncodedEvent> {
    events
        .into_iter()
        .filter_map(|me| match serde_json::to_string(&me) {
            Ok(json) => {
                // Serialize first (borrowing &me); the borrow ends when the
                // owned String returns, so we can move r#type out of `me`
                // instead of cloning it on the per-token encode hot path.
                let event_type = me.r#type;
                Some(ClientEncodedEvent::Anthropic { event_type, json })
            }
            Err(e) => {
                warn!(error = %e, phase, "failed to serialize Anthropic SSE event; dropping");
                None
            }
        })
        .collect()
}

/// Serialize OpenAI SSE chunks, dropping any that fail to serialize.
///
/// `phase` is used in log messages (e.g. "encode" vs "finish") for context.
fn wrap_openai_events<T: serde::Serialize>(chunks: Vec<T>, phase: &str) -> Vec<ClientEncodedEvent> {
    chunks
        .into_iter()
        .filter_map(|chunk| match serde_json::to_string(&chunk) {
            Ok(json) => Some(ClientEncodedEvent::OpenAi { json }),
            Err(e) => {
                warn!(error = %e, phase, "failed to serialize OpenAI SSE chunk; dropping");
                None
            }
        })
        .collect()
}

/// Convert a [`ClientEncodedEvent`] into an axum SSE [`Event`].
fn client_event_to_sse(encoded: ClientEncodedEvent) -> Event {
    match encoded {
        ClientEncodedEvent::Anthropic { event_type, json } => {
            Event::default().event(&event_type).data(json)
        }
        ClientEncodedEvent::OpenAi { json } => Event::default().data(json),
    }
}

/// Build the terminal `[DONE]` event for OpenAI Chat streams.
fn openai_done_event() -> Event {
    Event::default().data("[DONE]")
}

// ---------------------------------------------------------------------------
// RequestContext
// ---------------------------------------------------------------------------

/// Pre-resolved per-request context shared between non-streaming and streaming
/// paths.
#[derive(Debug)]
pub(crate) struct RequestContext {
    /// Unique request ID for tracing and the `x-request-id` header.
    pub(crate) request_id: String,
    /// Instant when the handler was entered (for latency metrics).
    pub(crate) start: Instant,
    /// Authenticated client key id, or `"anonymous"` under the loopback
    /// no-auth exemption. Stamped onto `RequestReceived` events.
    pub(crate) caller: String,
    /// SHA-256 of the raw client request body, for the `RequestReceived.body_hash`
    /// correlation key. Computed in [`prepare_request`] over the raw wire bytes
    /// (where they first arrive), per the plan's locked decision, so it is shared
    /// across the inference routes and `/v1/messages/count_tokens`.
    pub(crate) body_hash: String,
}

// ---------------------------------------------------------------------------
// prepare_request
// ---------------------------------------------------------------------------

/// Perform pre-flight checks and build a [`RequestContext`].
///
/// Returns `Err(RouteError)` early for rate-limited or duplicate requests so
/// the handler can immediately produce an error response without entering the
/// core pipeline.
///
/// `meta.request_id` is minted by the outermost `inject_request_id` layer and
/// `meta.caller` is stamped by the client-auth middleware, so the id attached
/// to the `x-request-id` header matches the id stamped on the request's
/// tracing span (audit MEDIUM-1). An authenticated request is rate-limited
/// under its key id, an anonymous one under its client IP.
pub(crate) fn prepare_request(
    state: &AppState,
    meta: &crate::middleware::RequestMeta,
    headers: &HeaderMap,
    body: &[u8],
    path: &str,
) -> Result<RequestContext, RouteError> {
    let caller = &meta.caller;
    let connect_info = meta.connect_info.as_ref();
    let trust_forwarded_headers = state.trust_forwarded_headers();
    let client_ip = get_client_ip(headers, connect_info, trust_forwarded_headers);
    // Security: loopback bypass is intentional for local development and testing.
    // When `trust_forwarded_headers` is false, the connection-info IP is guaranteed
    // to be the actual TCP peer (not spoofable via headers). This means only
    // processes on the same machine can bypass rate limiting. When
    // `trust_forwarded_headers` is true, the loopback check is disabled because
    // the peer IP may be spoofed by an intermediate reverse proxy.
    //
    // The bypass applies only to unauthenticated callers: anonymous requests
    // exist solely under the loopback-only `auth.mode = "none"`, so this
    // preserves today's local behavior, while a request authenticated with a
    // client key is always limited under its key id.
    let direct_loopback = !trust_forwarded_headers
        && !caller.is_authenticated()
        && connect_info.is_some_and(|address| address.ip().is_loopback());

    let rate_limit_key = if caller.is_authenticated() {
        caller.as_str().to_owned()
    } else {
        client_ip
    };
    if !direct_loopback && !state.rate_limiter.is_allowed(&rate_limit_key) {
        state.metrics.record_rate_limited();
        return Err(RouteError::RateLimited);
    }

    if state.request_dedup.is_duplicate_with_path(path, body) {
        state.metrics.record_deduplicated();
        return Err(RouteError::Conflict);
    }

    Ok(RequestContext {
        request_id: meta.request_id.0.clone(),
        start: Instant::now(),
        caller: caller.as_str().to_owned(),
        body_hash: body_hash_of_bytes(body),
    })
}

// ---------------------------------------------------------------------------
// resolve target helper
// ---------------------------------------------------------------------------

/// Maximum length for a provider name in URL path parameters.
///
/// Prevents regex DoS from arbitrarily long names. 128 characters is generous
/// enough for any realistic provider identifier.
const MAX_PROVIDER_NAME_LEN: usize = 128;

/// Regex for validating provider names in URL paths.
///
/// Provider names must be ASCII lowercase letters, digits, hyphens, and
/// underscores. This ensures URL-safe slugs that cannot be confused with path
/// traversal or encoding attacks.
static PROVIDER_NAME_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^[a-z0-9_-]+$").expect("valid provider name regex")
});

/// Validate that a provider name from a URL path contains only safe characters.
pub(super) fn validate_provider_name(name: &str) -> Result<(), RouteError> {
    if name.is_empty() {
        return Err(RouteError::InvalidProviderName(
            "provider name is empty".to_owned(),
        ));
    }
    if name.len() > MAX_PROVIDER_NAME_LEN {
        return Err(RouteError::InvalidProviderName(format!(
            "provider name exceeds maximum length of {MAX_PROVIDER_NAME_LEN} characters"
        )));
    }
    if !PROVIDER_NAME_REGEX.is_match(name) {
        return Err(RouteError::InvalidProviderName(format!(
            "provider name \"{name}\" contains invalid characters; \
             only lowercase ASCII letters, digits, hyphens, and underscores are allowed"
        )));
    }
    Ok(())
}

/// Reject a request whose `Content-Type` is not JSON before attempting to parse
/// the body, so a non-JSON body yields a precise "expected application/json"
/// 400 rather than a misleading "invalid JSON" parse error (audit LOW-30).
///
/// Handlers accept `axum::body::Bytes` (so the raw body can be hashed for
/// dedup), which bypasses the `Json` extractor's built-in content-type check.
/// This restores the spirit of that check uniformly across every JSON POST
/// handler (`chat`, `messages`, `token_count`). A missing `Content-Type` is
/// tolerated for backwards compatibility, matching the prior behaviour.
pub(super) fn validate_json_content_type(headers: &HeaderMap) -> Result<(), RouteError> {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return Ok(());
    };
    let Ok(ct) = value.to_str() else {
        return Ok(());
    };
    // Accept `application/json` and any `+json` suffix (e.g.
    // `application/vnd.api+json`). Strip any `; charset=...` parameters first.
    let essence = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    let is_json = essence == "application/json" || essence.ends_with("+json");
    if !is_json {
        return Err(RouteError::InvalidRequest(format!(
            "expected application/json Content-Type, got {ct}"
        )));
    }
    Ok(())
}

/// Validate a provider name extracted from the URL path.
///
/// This is a thin wrapper around [`validate_provider_name`] used at handler
/// entry points as defense-in-depth. Axum's `Path<String>` extractor does not
/// perform character validation, so this ensures the provider name is safe
/// before it is used in format strings or lookups.
fn validate_provider_name_ref(provider_name: &str) -> Result<&str, RouteError> {
    validate_provider_name(provider_name)?;
    Ok(provider_name)
}

/// Resolve a provider route through the provider registry into a tuple of
/// (`ProviderAdapterTarget`, `ProviderAdapter`) ready for encoding.
///
/// Lookup chain:
///
/// ```text
/// provider_name + route_kind  -> provider config + route -> adapter name
/// adapter_name                -> protocol + endpoint + headers
/// requested_model             -> model_aliases -> upstream_model
/// ```
async fn resolve_target(
    state: &AppState,
    provider_name: &str,
    route_kind: ProviderRouteKind,
    core: &CoreRequest,
    inbound_auth: Option<&SecretString>,
) -> Result<(ProviderAdapterTarget, ProviderAdapter), RouteError> {
    let providers = state.providers();

    let adapter_target_config = providers
        .resolve_provider_route(provider_name, route_kind, &core.model.requested)
        .map_err(map_provider_route_error)?;

    // Catalog enforcement: fetch the provider once and check both the
    // enforcement flag and the model catalog in a single lookup.
    if let Some(provider) = providers.get(provider_name) {
        if provider
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog.enforce)
        {
            // O(1) membership check backed by a cached id index inside
            // ModelCatalogService (audit LOW-9); replaces a per-request O(N)
            // linear scan over the merged catalog.
            let allowed = state
                .model_catalogs()
                .contains_model(
                    provider,
                    &adapter_target_config.upstream_model,
                    &adapter_target_config.protocol,
                )
                .await
                .map_err(|error| RouteError::Internal(error.to_string()))?;
            if !allowed {
                return Err(RouteError::ModelNotAllowed(
                    adapter_target_config.upstream_model.clone(),
                ));
            }
        }
    }

    let protocol = ProviderProtocol::parse(&adapter_target_config.protocol).ok_or_else(|| {
        RouteError::Internal(format!(
            "unknown protocol: {}",
            adapter_target_config.protocol
        ))
    })?;

    let adapter = state
        .provider_adapters
        .get(protocol)
        .ok_or_else(|| {
            RouteError::Internal(format!(
                "no adapter registered for protocol: {}",
                adapter_target_config.protocol
            ))
        })?
        // ProviderAdapter variants are zero-sized unit structs; clone is a
        // trivial copy. (The Arc sits one level out, on
        // `provider_adapters: Arc<ProviderAdapterRegistry>`; `get()` returns a
        // borrowed `&ProviderAdapter`, and this `.clone()` copies only the
        // zero-sized enum value out from under the borrow — no refcount bump.)
        .clone();

    // Resolve the upstream api_key: passthrough-auth providers use the client's
    // inbound token (401 if absent); others use the configured static key.
    let api_key = if providers
        .get(provider_name)
        .is_some_and(|p| p.passthrough_auth)
    {
        match inbound_auth {
            Some(token) => token.clone(),
            None => return Err(RouteError::Unauthorized),
        }
    } else {
        adapter_target_config.api_key
    };

    let provider_target = ProviderAdapterTarget {
        provider_name: adapter_target_config.provider_name,
        adapter_name: adapter_target_config.adapter_name,
        protocol,
        endpoint: adapter_target_config.endpoint,
        auth_style: adapter_target_config.auth_style,
        api_key,
        requested_model: adapter_target_config.requested_model,
        upstream_model: adapter_target_config.upstream_model,
        headers: adapter_target_config.headers,
    };

    Ok((provider_target, adapter))
}

fn map_provider_route_error(error: ProviderRouteResolutionError) -> RouteError {
    match error {
        ProviderRouteResolutionError::UnknownProvider { provider } => {
            RouteError::UnknownProvider(provider)
        }
        error @ ProviderRouteResolutionError::UnsupportedRoute { .. } => {
            RouteError::UnsupportedRoute(error.to_string())
        }
        error => RouteError::Internal(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// handle_core_once
// ---------------------------------------------------------------------------

/// Canonical string for a [`ProviderRouteKind`] (used in the event log).
fn route_kind_str(kind: ProviderRouteKind) -> &'static str {
    match kind {
        ProviderRouteKind::ChatCompletions => "chat_completions",
        ProviderRouteKind::Messages => "messages",
    }
}

/// Canonical string for a [`ClientProtocol`] (used in the event log).
fn client_protocol_str(protocol: ClientProtocol) -> &'static str {
    match protocol {
        ClientProtocol::OpenAiChat => "openai_chat",
        ClientProtocol::Anthropic => "anthropic",
    }
}

/// Stable correlation hash of the RAW client request body, hex-encoded.
///
/// SHA-256 of the raw wire bytes (`&[u8]`) exactly as the client sent them. The
/// `/v1/messages/count_tokens` route uses the same input, so a request has the
/// same `body_hash` correlation key across every route that emits a
/// `RequestReceived`. The raw body itself is never stored -- only this digest.
///
/// Shared with `token_count.rs` so the two routes agree on the correlation key
/// (audit finding: body_hash was hashed over the normalized `CoreRequest` on the
/// inference routes but raw bytes on token_count, breaking cross-route
/// correlation).
pub(crate) fn body_hash_of_bytes(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    format!("{:x}", hasher.finalize())
}

/// Emit a structured `ResponseFailed` event for a route error. Borrows the
/// error so the caller can still return it; derives HTTP status + client-facing
/// error kind from the canonical [`extract_error_fields`] mapping.
///
/// When the caller does not supply a `provider` (the early-gate failure path,
/// where the request never reached a resolved provider), the provider name is
/// recovered from [`RouteError::UnknownProvider`] so the event still records
/// which provider name was probed. This keeps the unknown-provider failure
/// attributable in the event log without each early-gate caller having to
/// thread the name through separately.
pub(crate) fn emit_response_failed(
    event_bus: &Arc<dyn EventBus>,
    request_id: &str,
    provider: Option<&str>,
    model: Option<&ModelRef>,
    error: &RouteError,
    start: Instant,
) {
    let provider = provider.or(match error {
        RouteError::UnknownProvider(name) => Some(name.as_str()),
        _ => None,
    });
    let (status, _client_error_type, message) = extract_error_fields(error);
    // Log the (already key/URL-redacted) upstream body exactly once here. This
    // is the single canonical failure-recording site, guaranteed to run once
    // per failure; emitting it inside `extract_error_fields` instead would
    // double-warn because that helper is also called from `route_error_response`
    // on the render path.
    if let RouteError::Upstream {
        status: upstream_status,
        body,
        auth_owner,
    } = error
    {
        tracing::warn!(
            upstream_status = upstream_status.as_u16(),
            mapped_status = status.as_u16(),
            auth_owner = ?auth_owner,
            upstream_body = %body,
            "upstream error mapped for the client; real body logged here only"
        );
    }
    // `error_kind` records the Rust variant name (the documented event-log
    // discriminant), NOT the client-facing protocol error-type string, so
    // distinct variants stay separable in the event log (audit finding:
    // error_kind not variant name).
    let error_kind = route_error_variant_name(error);
    event_bus.emit(&ProxyEvent::ResponseFailed(ResponseFailed {
        request_id: request_id.to_owned(),
        timestamp: time::OffsetDateTime::now_utc(),
        provider: provider.map(str::to_owned),
        model: model.cloned(),
        error_kind: error_kind.to_owned(),
        message,
        http_status: status.as_u16(),
        latency_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
    }));
}

/// Capture lifecycle fields from a [`CoreEvent`] into the stream context
/// (upstream message id, cumulative usage, stop reason). Called on every event
/// before it is consumed by `encode_core_event`, so the values survive to the
/// stream-completion `ResponseCompleted` emission.
fn capture_lifecycle(ctx: &mut StreamContext, event: &CoreEvent) {
    match event {
        CoreEvent::MessageStart { id: Some(id), .. } => {
            ctx.upstream_message_id = Some(id.clone());
        }
        CoreEvent::UsageDelta { usage } => {
            // Providers send a cumulative running total, so replace (do not sum).
            ctx.pending_usage = Some(usage.clone());
        }
        CoreEvent::MessageStop { stop_reason, .. } => {
            ctx.stop_reason = Some(stop_reason.clone());
        }
        _ => {}
    }
}

/// Extract the client's inbound auth token from request headers, for passthrough
/// providers. Checks `Authorization: Bearer <token>`, then `x-api-key`, then
/// `x-goog-api-key`. The raw token is wrapped in [`SecretString`] so it is
/// redacted in logs/Debug like any other credential.
pub(crate) fn extract_inbound_auth(headers: &HeaderMap) -> Option<SecretString> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            // The auth scheme is case-insensitive (RFC 7235): "Bearer",
            // "bearer", "BEARER", "bEaReR" are all valid. Split on the first
            // space to separate the scheme from the token and compare the
            // scheme case-insensitively, rather than matching only two casings.
            let mut parts = s.splitn(2, ' ');
            match (parts.next(), parts.next()) {
                (Some(scheme), Some(token)) if scheme.eq_ignore_ascii_case("bearer") => {
                    let token = token.trim();
                    if !token.is_empty() {
                        return Some(SecretString::from(token.to_owned()));
                    }
                }
                _ => {}
            }
        }
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(value) = headers.get(name) {
            if let Ok(s) = value.to_str() {
                if !s.is_empty() {
                    return Some(SecretString::from(s.to_owned()));
                }
            }
        }
    }
    None
}

/// Compute the USD cost of a request from its [`Usage`] and a per-token price
/// table.
///
/// Pure domain math. Lives in the server crate (not on the protocol [`Cost`]
/// type) because [`Usage`] is in the protocol crate and [`ModelPricing`] in the
/// core crate, and the server is the first crate that depends on both.
///
/// # Overflow safety
///
/// `rust_decimal`'s `*`/`+` operators PANIC on overflow. Token counts (up to
/// ~2.1e9) multiplied by a price, then summed across five components, can
/// exceed `Decimal`'s 96-bit range, which would panic the request handler. We
/// use checked arithmetic and saturate to `Decimal::MAX` instead, so a bad
/// (or just very large) price/config can never crash a cost-bearing request.
/// Config validation also bounds prices (see `MAX_PER_TOKEN_PRICE`); the
/// checked math here is the belt-and-suspenders guard.
///
/// # Rounding
///
/// Per the plan's locked decision 5 ("rounded to the nearest microcent on
/// output only"), each component is rounded to 6 decimal places (a microcent
/// of USD) after the multiply. The total is the checked sum of the rounded
/// components, so it is always exactly their sum (no separate rounding drift).
fn compute_cost(usage: &Usage, pricing: &ModelPricing) -> Cost {
    /// Multiply a token count by a per-token price, saturating on overflow,
    /// then round to the nearest microcent (6 dp).
    fn priced(count: i32, price: Decimal) -> Decimal {
        Decimal::from(count)
            .checked_mul(price)
            .unwrap_or(Decimal::MAX)
            .round_dp(6)
    }
    /// Checked sum that saturates to `Decimal::MAX` on overflow.
    fn sum(a: Decimal, b: Decimal) -> Decimal {
        a.checked_add(b).unwrap_or(Decimal::MAX)
    }

    let input = priced(usage.input_tokens, pricing.input);
    let output = priced(usage.output_tokens, pricing.output);
    let cache_creation = priced(
        usage.cache_creation_input_tokens.unwrap_or(0),
        pricing.cache_creation,
    );
    let cache_read = priced(
        usage.cache_read_input_tokens.unwrap_or(0),
        pricing.cache_read,
    );
    let reasoning = priced(usage.reasoning_tokens.unwrap_or(0), pricing.reasoning);
    let total = [input, output, cache_creation, cache_read, reasoning]
        .into_iter()
        .reduce(sum)
        .unwrap_or(Decimal::ZERO);
    Cost {
        input,
        output,
        cache_creation,
        cache_read,
        reasoning,
        total,
    }
}

/// Resolved dispatch parameters shared by [`handle_core_once`] and
/// [`handle_core_stream`]: which provider the URL named, which route kind is
/// being served, and which client protocol the response must be encoded into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoreDispatch<'a> {
    /// Provider name from the URL path.
    pub(crate) provider_name: &'a str,
    /// Route kind (`chat_completions` or `messages`).
    pub(crate) route_kind: ProviderRouteKind,
    /// Client protocol for the response encoding.
    pub(crate) client_protocol: ClientProtocol,
}

/// Non-streaming core pipeline.
///
/// ```text
/// CoreRequest -> encode -> send -> decode -> client encode -> HTTP response
/// ```
#[expect(
    clippy::too_many_lines,
    reason = "the pipeline stages run in request order and each stage's \
              metric + ResponseFailed emission sits next to the call that can \
              fail; extracting stages would separate the failure bookkeeping \
              from the step it belongs to"
)]
pub(crate) async fn handle_core_once(
    state: AppState,
    ctx: RequestContext,
    dispatch: CoreDispatch<'_>,
    core: CoreRequest,
    inbound_auth: Option<SecretString>,
) -> Result<Response<Body>, RouteError> {
    let CoreDispatch {
        provider_name,
        route_kind,
        client_protocol,
    } = dispatch;
    validate_provider_name_ref(provider_name)?;

    state.metrics.record_request(false);

    // Whether the client's own token is forwarded to the upstream
    // (passthrough_auth). Captured once here so error classification
    // (map_provider_error) can mark auth/permission failures as client-owned
    // and pass 401/403 through verbatim instead of collapsing to 502
    // (audit LOW, passthrough-auth-401-collapse).
    let passthrough_auth = state
        .providers()
        .get(provider_name)
        .is_some_and(|p| p.passthrough_auth);

    tracing::debug!(
        request_id = %ctx.request_id,
        provider = %provider_name,
        model = %core.model.requested,
        streaming = false,
        "processing request"
    );

    // Emit a structured RequestReceived event.
    state
        .event_bus
        .emit(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: ctx.caller.clone(),
            provider: provider_name.to_owned(),
            route_kind: route_kind_str(route_kind).to_owned(),
            client_protocol: client_protocol_str(client_protocol).to_owned(),
            model: core.model.clone(),
            streaming: false,
            body_hash: ctx.body_hash.clone(),
        }));

    let (target, adapter) = resolve_target(
        &state,
        provider_name,
        route_kind,
        &core,
        inbound_auth.as_ref(),
    )
    .await
    .inspect_err(|e| {
        state.metrics.record_failure();
        emit_response_failed(
            &state.event_bus,
            &ctx.request_id,
            Some(provider_name),
            Some(&core.model),
            e,
            ctx.start,
        );
    })?;

    tracing::debug!(
        request_id = %ctx.request_id,
        provider = %target.provider_name,
        upstream_model = %target.upstream_model,
        "routed to provider"
    );

    // Encode the core request into a provider-specific HTTP request.
    // All encode errors map to Internal per the plan's error behavior spec:
    // the core pipeline has already validated the request at this point, so
    // any encode failure is a proxy/adapter issue, not a client error.
    let proxy_req: ProxyRequest = adapter.encode_request(&core, &target).map_err(|e| {
        let err = RouteError::Internal(format!("encode error: {e}"));
        state.metrics.record_failure();
        emit_response_failed(
            &state.event_bus,
            &ctx.request_id,
            Some(provider_name),
            Some(&core.model),
            &err,
            ctx.start,
        );
        err
    })?;

    let response_bytes = state.proxy_client.send(proxy_req).await.map_err(|e| {
        let err = map_provider_error(&e, passthrough_auth);
        state.metrics.record_failure();
        emit_response_failed(
            &state.event_bus,
            &ctx.request_id,
            Some(provider_name),
            Some(&core.model),
            &err,
            ctx.start,
        );
        err
    })?;

    // Decode the provider response into a CoreResponse.
    let mut core_resp = adapter
        .decode_response(&response_bytes, &target)
        .map_err(|e| {
            let err = RouteError::ProviderDecode(format!("decode response: {e}"));
            state.metrics.record_failure();
            emit_response_failed(
                &state.event_bus,
                &ctx.request_id,
                Some(provider_name),
                Some(&core.model),
                &err,
                ctx.start,
            );
            err
        })?;

    // Encode into the client-specific response.
    let latency = ctx.start.elapsed();

    // Attach computed cost when pricing is configured for the (alias-resolved)
    // upstream model. The price is cloned out of the registry before the
    // `core_resp` move into the client encoder below.
    let pricing = state
        .providers()
        .pricing_for(&target.provider_name, &target.upstream_model)
        .cloned();
    // Only attach a cost when the usage was genuinely reported by the provider.
    // Synthetic/Unknown usage (zero-filled because the provider omitted it)
    // would otherwise produce a Some(zero-Cost) indistinguishable from a truly
    // free request; leaving cost None for unprovenanced usage keeps the event
    // honest (audit INFO, cost-on-synthetic-zero).
    let cost = if core_resp.usage.provenance == UsageProvenance::ProviderReported {
        pricing.map(|p| compute_cost(&core_resp.usage, &p))
    } else {
        None
    };
    core_resp.cost = cost.clone();
    // Capture the ResponseCompleted payload before `core_resp` is moved into the
    // client encoder; the event is emitted AFTER a successful encode so we never
    // log "completed" for a request that then fails client encoding.
    let resp_usage = core_resp.usage.clone();
    let resp_model = core_resp.model.clone();
    let resp_upstream_id = core_resp.id.clone();
    let resp_stop_reason = core_resp.stop_reason.clone();

    // Client-encode failures map to Internal. They must record a failure metric
    // AND emit a ResponseFailed event (mirroring the decode/send arms above),
    // and must NOT be counted as a success. `record_success` is therefore
    // deferred until AFTER this match succeeds (audit LOW, eventlog-04).
    let response_body = match client_protocol {
        ClientProtocol::Anthropic => {
            let msg_resp = anthropic::encode_response(core_resp).map_err(|e| {
                let err = RouteError::Internal(format!("client encode: {e}"));
                state.metrics.record_failure();
                emit_response_failed(
                    &state.event_bus,
                    &ctx.request_id,
                    Some(provider_name),
                    Some(&resp_model),
                    &err,
                    ctx.start,
                );
                err
            })?;
            serde_json::to_vec(&msg_resp).map_err(|e| {
                let err = RouteError::Internal(format!("serialize: {e}"));
                state.metrics.record_failure();
                emit_response_failed(
                    &state.event_bus,
                    &ctx.request_id,
                    Some(provider_name),
                    Some(&resp_model),
                    &err,
                    ctx.start,
                );
                err
            })?
        }
        ClientProtocol::OpenAiChat => {
            let chat_resp = openai_chat::encode_response(core_resp).map_err(|e| {
                let err = RouteError::Internal(format!("client encode: {e}"));
                state.metrics.record_failure();
                emit_response_failed(
                    &state.event_bus,
                    &ctx.request_id,
                    Some(provider_name),
                    Some(&resp_model),
                    &err,
                    ctx.start,
                );
                err
            })?;
            serde_json::to_vec(&chat_resp).map_err(|e| {
                let err = RouteError::Internal(format!("serialize: {e}"));
                state.metrics.record_failure();
                emit_response_failed(
                    &state.event_bus,
                    &ctx.request_id,
                    Some(provider_name),
                    Some(&resp_model),
                    &err,
                    ctx.start,
                );
                err
            })?
        }
    };

    // Client encoding succeeded: record the success metric now (after the
    // encode match above), so an encode failure is never counted as success
    // (audit LOW, eventlog-04). The stream path already defers this to
    // `stream_succeeded`.
    state
        .metrics
        .record_success(&target.provider_name, &target.upstream_model, latency);

    // Client encoding succeeded: emit the ResponseCompleted event.
    state
        .event_bus
        .emit(&ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: target.provider_name.clone(),
            upstream_message_id: resp_upstream_id,
            model: resp_model,
            usage: resp_usage,
            cost,
            stop_reason: resp_stop_reason,
            latency_ms: latency.as_millis().try_into().unwrap_or(u64::MAX),
        }));

    tracing::debug!(
        request_id = %ctx.request_id,
        model = %target.upstream_model,
        latency_ms = latency.as_millis(),
        "request completed"
    );

    let mut response = (StatusCode::OK, Body::from(response_body)).into_response();

    // Insert the request ID header safely -- the ID is dynamically generated
    // so we use from_str with a fallback rather than expect/unwrap.
    response.headers_mut().insert(
        "x-request-id",
        ctx.request_id
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")),
    );

    // Set content-type explicitly (defense-in-depth).
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );

    Ok(response)
}

// ---------------------------------------------------------------------------
// handle_core_stream
// ---------------------------------------------------------------------------

/// Heartbeat interval for SSE streams.
///
/// Chosen as 3 seconds to keep Anthropic SDK clients from timing out on
/// long-running streams while avoiding excessive bandwidth overhead.
/// TODO(future): Make this configurable via AppState or TOML config.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

/// Message sent from the spawned stream task to the handler during the
/// first-byte probe phase. This enables the handler to return a proper
/// HTTP error status if the stream fails before emitting any data.
enum FirstByteResult {
    /// At least one SSE event was produced by the stream. The stream is now
    /// live and all subsequent events/errors must be delivered as in-band
    /// SSE events (the HTTP status is already committed as 200 OK).
    FirstEvent(Event),
    /// The stream failed before producing any event. The handler should
    /// return an HTTP error response (e.g. 502 Bad Gateway) instead of
    /// committing a 200 OK SSE response.
    PreStreamError(RouteError),
}

/// Streaming core pipeline.
///
/// ```text
/// CoreRequest -> encode -> send_stream -> SseFramer -> ProviderStreamDecoder
///            -> StreamEncoder -> SSE response
/// ```
///
/// ## first_byte_sent tracking
///
/// The plan requires that errors before the first data byte be returned as
/// HTTP errors (e.g. 502), not as in-band SSE error events. This is
/// implemented by probing for the first event before committing the HTTP 200
/// response: the spawned task sends the first event (or an error) back to
/// the handler via a oneshot channel, and the handler decides whether to
/// return an HTTP error or an SSE response.
#[expect(
    clippy::too_many_lines,
    reason = "mirrors handle_core_once for the streaming path: stage order is \
              the audit trail and each stage's failure bookkeeping stays next \
              to the failing call"
)]
pub(crate) async fn handle_core_stream(
    state: AppState,
    ctx: RequestContext,
    dispatch: CoreDispatch<'_>,
    core: CoreRequest,
    inbound_auth: Option<SecretString>,
) -> Result<Response<Body>, RouteError> {
    let CoreDispatch {
        provider_name,
        route_kind,
        client_protocol,
    } = dispatch;
    validate_provider_name_ref(provider_name)?;

    state.metrics.record_request(true);

    // Whether the client's own token is forwarded to the upstream
    // (passthrough_auth). Captured once here so error classification
    // (map_provider_error, including inside the spawned stream task) can mark
    // auth/permission failures as client-owned and pass 401/403 through verbatim
    // instead of collapsing to 502 (audit LOW, passthrough-auth-401-collapse).
    let passthrough_auth = state
        .providers()
        .get(provider_name)
        .is_some_and(|p| p.passthrough_auth);

    tracing::debug!(
        request_id = %ctx.request_id,
        provider = %provider_name,
        model = %core.model.requested,
        streaming = true,
        "processing streaming request"
    );

    // Emit a structured RequestReceived event BEFORE resolve_target/send_stream
    // so a failure in either step still has a preceding RequestReceived (the
    // non-stream path emits at the same point). Without this, an early-return
    // from resolve_target/send_stream would emit an orphaned ResponseFailed with
    // no matching RequestReceived (audit LOW, eventlog-03).
    state
        .event_bus
        .emit(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: ctx.caller.clone(),
            provider: provider_name.to_owned(),
            route_kind: route_kind_str(route_kind).to_owned(),
            client_protocol: client_protocol_str(client_protocol).to_owned(),
            model: core.model.clone(),
            streaming: true,
            body_hash: ctx.body_hash.clone(),
        }));

    let (target, adapter) = resolve_target(
        &state,
        provider_name,
        route_kind,
        &core,
        inbound_auth.as_ref(),
    )
    .await
    .inspect_err(|e| {
        state.metrics.record_failure();
        emit_response_failed(
            &state.event_bus,
            &ctx.request_id,
            Some(provider_name),
            Some(&core.model),
            e,
            ctx.start,
        );
    })?;

    tracing::debug!(
        request_id = %ctx.request_id,
        provider = %target.provider_name,
        upstream_model = %target.upstream_model,
        "routed to provider (streaming)"
    );

    // Encode the core request into a provider-specific HTTP request.
    // All encode errors map to Internal per the plan's error behavior spec:
    // the core pipeline has already validated the request at this point.
    let proxy_req: ProxyRequest = adapter.encode_request(&core, &target).map_err(|e| {
        let err = RouteError::Internal(format!("encode error: {e}"));
        state.metrics.record_failure();
        emit_response_failed(
            &state.event_bus,
            &ctx.request_id,
            Some(provider_name),
            Some(&core.model),
            &err,
            ctx.start,
        );
        err
    })?;

    // Open the streaming connection.
    let byte_stream = state
        .proxy_client
        .send_stream(proxy_req)
        .await
        .map_err(|e| {
            let err = map_provider_error(&e, passthrough_auth);
            state.metrics.record_failure();
            emit_response_failed(
                &state.event_bus,
                &ctx.request_id,
                Some(provider_name),
                Some(&core.model),
                &err,
                ctx.start,
            );
            err
        })?;

    let provider_decoder = adapter.new_stream_decoder(&target);
    let sse_framer = SseFramer::new();

    // The client stream encoder is constructed *inside* the spawned stream
    // task (see `build_sse_output_stream`), after buffering the first decoded
    // `CoreEvent`. This lets the encoder be seeded with the upstream provider's
    // real message ID (from `CoreEvent::MessageStart`) instead of a synthetic
    // one, so clients tracking message IDs see the upstream's own ID rather than
    // a proxy-generated one.

    // ctx is consumed after this point. request_id is cloned once for the
    // spawned task and once for the response header (both are needed).
    let request_id = ctx.request_id;
    let upstream_model = target.upstream_model.clone();

    // Pricing for the resolved upstream model (None => no cost on the event).
    let pricing = state
        .providers()
        .pricing_for(&target.provider_name, &target.upstream_model)
        .cloned();
    let event_bus = Arc::clone(&state.event_bus);

    // Capture fields needed for a stream ResponseFailed event, before `core`
    // and `ctx.start` are moved into the StreamContext below.
    let requested_model = core.model.clone();
    let stream_start = ctx.start;

    // Build the output SSE stream with first-byte tracking.
    let (first_byte_tx, first_byte_rx) = tokio::sync::oneshot::channel::<FirstByteResult>();
    // Shared "stream failure already recorded" guard. A pre-first-byte panic is
    // observed by TWO independent watchers: this handler (the `first_byte_rx`
    // `RecvError` path, since the panicked task drops the sender) AND the
    // detached supervisor (the `JoinError` path). Without coordination each
    // would emit its own `ResponseFailed` and bump the failure metric for the
    // same request. The first watcher to run claims the flag via `swap`; the
    // other skips. Exactly one terminal event + one failure metric per request
    // (audit finding: stream-task panic double-ResponseFailed).
    let failure_recorded = Arc::new(AtomicBool::new(false));
    let output_stream = build_sse_output_stream(
        byte_stream,
        StreamContext {
            provider_decoder,
            sse_framer,
            client_protocol,
            core,
            request_id: request_id.clone(),
            stream_metrics: StreamMetrics {
                metrics: Arc::clone(&state.metrics),
                provider_name: target.provider_name.clone(),
                upstream_model: upstream_model.clone(),
                start: stream_start,
            },
            first_byte_tx: Some(first_byte_tx),
            first_byte_sent: false,
            pricing,
            passthrough_auth,
            event_bus,
            pending_usage: None,
            stop_reason: None,
            upstream_message_id: None,
        },
        Arc::clone(&failure_recorded),
    );

    // Wait for the first event (or a pre-stream error). This is the
    // first_byte_sent boundary: errors before this point become HTTP
    // errors; errors after this point become in-band SSE error events.
    let first_event = first_byte_rx.await.map_err(|error| {
        let err = RouteError::Internal(format!(
            "stream task exited before first event ({error}: possible panic, cancellation, or empty stream)"
        ));
        // Only record/emit if the supervisor has not already done so for this
        // request (see `failure_recorded` above).
        if !failure_recorded.swap(true, Ordering::SeqCst) {
            state.metrics.record_failure();
            emit_response_failed(
                &state.event_bus,
                &request_id,
                Some(target.provider_name.as_str()),
                Some(&requested_model),
                &err,
                stream_start,
            );
        }
        err
    })?;

    match first_event {
        FirstByteResult::PreStreamError(route_error) => {
            // Stream failed before emitting any data. Emit a ResponseFailed
            // event (covers every in-task pre-stream failure funnelled through
            // the first-byte channel), then return as an HTTP error.
            emit_response_failed(
                &state.event_bus,
                &request_id,
                Some(target.provider_name.as_str()),
                Some(&requested_model),
                &route_error,
                stream_start,
            );
            Err(route_error)
        }
        FirstByteResult::FirstEvent(event) => {
            // First event received. Commit HTTP 200 and start streaming.
            // Prepend the first event to the output stream.
            let stream =
                futures::stream::once(async move { Ok::<_, std::convert::Infallible>(event) })
                    .chain(output_stream.map(Ok::<_, std::convert::Infallible>));

            let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(HEARTBEAT_INTERVAL));

            let response = sse.into_response();
            let (mut parts, body) = response.into_parts();

            // Add custom headers.
            // Defense-in-depth: explicitly set Content-Type even though
            // Sse::into_response() sets it internally. This guards against
            // axum version changes and makes the intent explicit.
            parts.headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            // Prevent proxy/CDN caching of streaming responses.
            parts.headers.insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            );
            // Defense-in-depth: explicitly request persistent connection for
            // reverse-proxy scenarios. axum adds this automatically for HTTP/1.1
            // but some reverse proxies strip it unless explicitly set.
            parts.headers.insert(
                header::CONNECTION,
                axum::http::HeaderValue::from_static("keep-alive"),
            );
            parts.headers.insert(
                "x-accel-buffering",
                axum::http::HeaderValue::from_static("no"),
            );
            parts.headers.insert(
                "x-request-id",
                request_id
                    .parse()
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")),
            );

            tracing::debug!(
                request_id = %request_id,
                model = %target.upstream_model,
                "streaming started"
            );

            Ok(Response::from_parts(parts, body))
        }
    }
}

// ---------------------------------------------------------------------------
// SSE output stream builder
// ---------------------------------------------------------------------------

/// Context for tracking metrics during a streaming response.
#[derive(Debug)]
struct StreamMetrics {
    /// Shared metrics recorder for success/failure/latency tracking.
    metrics: Arc<Metrics>,
    /// Provider name used as part of the composite metrics key.
    provider_name: String,
    /// Upstream model name used as part of the composite metrics key.
    upstream_model: String,
    /// Instant when the handler was entered (for latency measurement).
    start: Instant,
}

/// Mutable context used inside the spawned stream task.
///
/// Groups the state that was previously passed as individual parameters to
/// `build_sse_output_stream`, reducing the function's parameter count from 9
/// to a single struct plus the byte stream.
struct StreamContext {
    provider_decoder: ProviderStreamDecoderKind,
    sse_framer: SseFramer,
    client_protocol: ClientProtocol,
    /// The normalized core request. Held so the client stream encoder can be
    /// constructed inside the stream task -- after buffering the first event to
    /// extract the upstream message ID -- with access to `model.requested` and
    /// provider hints.
    core: CoreRequest,
    request_id: String,
    stream_metrics: StreamMetrics,
    first_byte_tx: Option<tokio::sync::oneshot::Sender<FirstByteResult>>,
    first_byte_sent: bool,
    /// Per-token pricing for the resolved upstream model, for stream cost.
    pricing: Option<ModelPricing>,
    /// Whether the client's own token was forwarded (passthrough_auth). Drives
    /// 401/403 pass-through vs 502 collapse in in-band/pre-stream error
    /// classification (audit LOW, passthrough-auth-401-collapse).
    passthrough_auth: bool,
    /// Event sink handle (the task emits ResponseCompleted at stream end).
    event_bus: Arc<dyn EventBus>,
    /// Latest cumulative usage seen via `CoreEvent::UsageDelta` (replaced, not
    /// summed -- providers send a running total).
    pending_usage: Option<Usage>,
    /// Stop reason captured from `CoreEvent::MessageStop`.
    stop_reason: Option<StopReason>,
    /// Upstream message id captured from `CoreEvent::MessageStart`.
    upstream_message_id: Option<String>,
}

impl StreamContext {
    /// Emit a single SSE event, handling the first-byte boundary.
    ///
    /// Before the first event is sent, events go through the `first_byte_tx`
    /// channel so the handler can return a proper HTTP status. After the first
    /// event, events go directly to the `tx` channel.
    ///
    /// Returns `true` if the event was successfully delivered (or the stream
    /// should continue), `false` if the task should exit.
    async fn emit_event(&mut self, event: Event, tx: &tokio::sync::mpsc::Sender<Event>) -> bool {
        if !self.first_byte_sent {
            if let Some(fb_tx) = self.first_byte_tx.take() {
                if fb_tx.send(FirstByteResult::FirstEvent(event)).is_ok() {
                    self.first_byte_sent = true;
                    return true;
                }
                // Receiver dropped -- handler is gone (client disconnected or
                // handler timed out). The failure is unactionable because the
                // handler has already returned, so we just record the metric
                // and stop the stream task.
                //
                // Client-initiated disconnect, not an upstream failure:
                // record a cancellation so it does not inflate the error-rate
                // SLO (audit MEDIUM-5).
                self.stream_metrics.metrics.record_client_cancel();
                return false;
            }
            // Unreachable today: `first_byte_tx` is consumed only by a
            // successful first-event send (which sets `first_byte_sent`) or by
            // `send_pre_stream_error`, and every `send_pre_stream_error` call
            // site is immediately followed by a `return`/`break`. Guard it
            // explicitly so a future refactor cannot silently drop a
            // client-visible event while reporting success: stop the task
            // loudly instead of dropping the event and returning `true`.
            warn!(
                request_id = %self.request_id,
                "emit_event: first-byte channel already consumed but no first \
                 byte sent; stopping stream task to avoid silently dropping an event"
            );
            return false;
        } else if tx.send(event).await.is_err() {
            // Channel receiver dropped -- the SSE handler task has exited
            // (client disconnect or timeout). No further events can be
            // delivered, so stop the stream task.
            //
            // Client disconnect, not an upstream failure (audit MEDIUM-5). Emit
            // a terminal `ClientCancelled` event so this `RequestReceived` is
            // not left orphaned in the event log (audit finding: buffering vs
            // main-loop disconnect event inconsistency -- a pre-first-byte
            // disconnect already emits via the PreStreamError path).
            self.stream_metrics.metrics.record_client_cancel();
            self.emit_client_cancelled();
            return false;
        }
        true
    }

    /// Emit a pre-stream error through the first-byte channel.
    fn send_pre_stream_error(&mut self, error: RouteError) -> bool {
        if let Some(fb_tx) = self.first_byte_tx.take() {
            fb_tx.send(FirstByteResult::PreStreamError(error)).is_ok()
        } else {
            false
        }
    }

    /// Emit a terminal `ResponseFailed` recording that the client disconnected
    /// after the first byte (mid-stream abort).
    ///
    /// This closes the streaming lifecycle: every `RequestReceived` for a stream
    /// that started gets a matching terminal event, so a client-aborted stream
    /// is attributable instead of orphaned. `error_kind` is `"ClientCancelled"`,
    /// distinct from genuine upstream failures, so operators can filter these
    /// out of error-rate analysis. The HTTP status is 200 (already committed at
    /// the first byte); the `error_kind` carries the cancellation semantics.
    /// Idempotent callers are fine, but this is intended to be the single
    /// terminal emit for a disconnect path.
    fn emit_client_cancelled(&self) {
        self.event_bus
            .emit(&ProxyEvent::ResponseFailed(ResponseFailed {
                request_id: self.request_id.clone(),
                timestamp: time::OffsetDateTime::now_utc(),
                provider: Some(self.stream_metrics.provider_name.clone()),
                model: Some(self.core.model.clone()),
                error_kind: "ClientCancelled".to_owned(),
                message: "client disconnected before the stream completed".to_owned(),
                http_status: StatusCode::OK.as_u16(),
                latency_ms: self
                    .stream_metrics
                    .start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            }));
    }
}

/// Emit the core events produced by a finalization step.
///
/// Returns `false` when the client disconnected, in which case the caller must
/// abandon the stream task.
async fn emit_finalization_events(
    ctx: &mut StreamContext,
    encoder: &mut ClientStreamEncoder,
    tx: &tokio::sync::mpsc::Sender<Event>,
    core_events: Vec<CoreEvent>,
) -> bool {
    for core_event in core_events {
        capture_lifecycle(ctx, &core_event);
        for event in encode_core_event(encoder, core_event) {
            if !ctx.emit_event(event, tx).await {
                return false;
            }
        }
    }
    true
}

/// Flush any trailing partial SSE frame left in the framer and emit the events
/// it decodes to.
///
/// Returns `false` when the client disconnected, in which case the caller must
/// abandon the stream task.
async fn finalize_sse_framer(
    ctx: &mut StreamContext,
    encoder: &mut ClientStreamEncoder,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> bool {
    let trailing_frames = match ctx.sse_framer.finish() {
        Ok(frames) => frames,
        Err(e) => {
            warn!(
                request_id = %ctx.request_id,
                error = %e,
                "SSE framer finish error"
            );
            return true;
        }
    };
    for frame in &trailing_frames {
        let core_events = match ctx.provider_decoder.decode_frame(frame) {
            Ok(events) => events,
            Err(e) => {
                warn!(
                    request_id = %ctx.request_id,
                    error = %e,
                    "provider decode error on trailing frame"
                );
                break;
            }
        };
        if !emit_finalization_events(ctx, encoder, tx, core_events).await {
            return false;
        }
    }
    true
}

/// Emit the client encoder's trailing events, plus the OpenAI `[DONE]`
/// terminator when the client protocol requires it.
///
/// Returns `false` when the client disconnected, in which case the caller must
/// abandon the stream task.
async fn emit_encoder_finish(
    ctx: &mut StreamContext,
    tx: &tokio::sync::mpsc::Sender<Event>,
    final_encoded_events: Vec<ClientEncodedEvent>,
) -> bool {
    for encoded in final_encoded_events {
        let event = client_event_to_sse(encoded);
        if !ctx.emit_event(event, tx).await {
            return false;
        }
    }

    // For OpenAI Chat, emit the [DONE] terminator after all chunks.
    if matches!(ctx.client_protocol, ClientProtocol::OpenAiChat)
        && !ctx.emit_event(openai_done_event(), tx).await
    {
        return false;
    }

    true
}

/// Build a stream that converts provider byte chunks into client SSE events.
///
/// The `first_byte_tx` channel is used to signal the first-byte boundary:
/// the spawned task sends either `FirstByteResult::FirstEvent` (once the
/// first SSE event is produced) or `FirstByteResult::PreStreamError` (if the
/// stream fails before any event). After the first event is sent, the
/// channel is dropped and all subsequent errors become in-band SSE events.
#[expect(
    clippy::too_many_lines,
    reason = "one streaming state machine: the select-loop's cancellation, \
              decode, first-byte, and disconnect transitions read as a unit; \
              splitting the arms into helpers would scatter the state they \
              mutate"
)]
fn build_sse_output_stream(
    byte_stream: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<Bytes, llm_proxy_provider::error::ProviderError>>
                + Send
                + 'static,
        >,
    >,
    ctx: StreamContext,
    failure_recorded: Arc<AtomicBool>,
) -> BoxStream<'static, Event> {
    // SSE channel buffer size. 256 events provides ~256 KB of headroom (typical
    // SSE events are ~1 KB). If the client reads slowly and the buffer fills,
    // backpressure is applied naturally via the tokio mpsc channel. A larger
    // buffer reduces the chance of blocking the stream processing task at the
    // cost of more memory per concurrent stream. This value could be made
    // configurable via AppState in a future release if tuning is needed.
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(256);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Capture the fields needed to observe a panic in the spawned stream task.
    // CatchPanicLayer (mod.rs) wraps only the request future, not detached
    // tasks, so a panic *after* the first byte crosses the boundary is
    // otherwise swallowed: `tx` drops, the SSE stream ends mid-flight, and no
    // ResponseFailed event / failure metric is recorded. We capture the
    // JoinHandle below and supervise it so a JoinError (panic) emits a
    // ResponseFailed event + records a failure (audit LOW, detached spawn).
    // The span instruments the spawned future so the runtime's panic log also
    // carries the request id.
    let supervise_request_id = ctx.request_id.clone();
    let supervise_event_bus = Arc::clone(&ctx.event_bus);
    let supervise_metrics = Arc::clone(&ctx.stream_metrics.metrics);
    let supervise_provider = ctx.stream_metrics.provider_name.clone();
    let supervise_model = ctx.core.model.clone();
    let supervise_start = ctx.stream_metrics.start;
    let stream_span = tracing::info_span!(
        "stream_task",
        request_id = %supervise_request_id,
    );

    let stream_task = tokio::spawn(
        async move {
        let mut stream = byte_stream;
        let mut ctx = ctx;
        let mut stream_succeeded = false;
        // Track whether the stream terminated due to an error so we can
        // skip the finalization path that follows the main loop.
        let mut stream_errored = false;

        // Buffer the first decoded event batch so the client stream encoder can
        // be seeded with the upstream provider's real message ID (from
        // `CoreEvent::MessageStart`) instead of a synthetic one. This runs
        // *inside* the spawned task so the cancellation `select!` covers the
        // first-frame wait: a client disconnect during buffering aborts cleanly
        // instead of blocking on the upstream.
        //
        // `CoreEvent`s accumulate across frames/chunks until the first non-empty
        // batch arrives (empty decodes -- e.g. `:keepalive` comments -- are
        // skipped), then the encoder is constructed and the buffered events are
        // replayed through it. No events are lost or reordered.
        let mut pending: Vec<CoreEvent> = Vec::new();
        'buffer: loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    // Client disconnected before the first event.
                    ctx.stream_metrics.metrics.record_client_cancel();
                    ctx.send_pre_stream_error(
                        RouteError::Internal("client disconnected before first byte".to_owned()),
                    );
                    return;
                }
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => {
                        let frames = match ctx.sse_framer.push_chunk(&bytes) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(
                                    request_id = %ctx.request_id,
                                    error = %e,
                                    "SSE framing error in stream"
                                );
                                ctx.stream_metrics.metrics.record_failure();
                                ctx.send_pre_stream_error(RouteError::ProviderDecode(
                                    format!("stream framing error: {e}"),
                                ));
                                return;
                            }
                        };
                        for frame in &frames {
                            match ctx.provider_decoder.decode_frame(frame) {
                                Ok(events) => pending.extend(events),
                                Err(e) => {
                                    warn!(
                                        request_id = %ctx.request_id,
                                        error = %e,
                                        "provider decode error in stream"
                                    );
                                    ctx.stream_metrics.metrics.record_failure();
                                    ctx.send_pre_stream_error(RouteError::ProviderDecode(
                                        format!("provider decode error: {e}"),
                                    ));
                                    return;
                                }
                            }
                        }
                        if !pending.is_empty() {
                            break 'buffer;
                        }
                    }
                    Some(Err(e)) => {
                        warn!(
                            request_id = %ctx.request_id,
                            error = %e,
                            "upstream stream error"
                        );
                        ctx.stream_metrics.metrics.record_failure();
                        ctx.send_pre_stream_error(map_provider_error(&e, ctx.passthrough_auth));
                        return;
                    }
                    None => break 'buffer,
                }
            }
        }

        // Seed the encoder with the real upstream message ID when the buffered
        // batch contained a `MessageStart { id: Some(..) }`; otherwise fall back
        // to the protocol-conventional synthetic id.
        let msg_id =
            extract_upstream_message_id(&pending).unwrap_or_else(|| match ctx.client_protocol {
                ClientProtocol::OpenAiChat => format!("chatcmpl-{}", uuid::Uuid::new_v4()),
                ClientProtocol::Anthropic => format!("msg_{}", uuid::Uuid::new_v4()),
            });
        let mut encoder = ClientStreamEncoder::new(
            ctx.client_protocol,
            msg_id,
            ctx.core.model.requested.clone(),
            &ctx.core,
        );

        // Replay the buffered events. The first one crosses the first-byte
        // boundary (committing HTTP 200 via the `first_byte` channel); the rest
        // flow directly to the output channel.
        //
        // Special case: a leading `CoreEvent::Error` on the OpenAI path. The
        // OpenAI StreamEncoder returns `Err` for `CoreEvent::Error`, so
        // `encode_core_event` yields no SSE event and the error would be
        // silently dropped -- leaving `first_byte_sent` false and surfacing a
        // misleading "empty stream" 502 (audit LOW, stream-ids-01). When that
        // happens, synthesize the OpenAI error-object SSE event from the
        // CoreStreamError so it crosses the boundary as a real in-band event
        // (200 + error + [DONE]), mirroring the post-first-byte
        // `emit_stream_error` path. Anthropic encodes `Error` fine, so this
        // branch is OpenAI-only.
        for core_event in pending.drain(..) {
            capture_lifecycle(&mut ctx, &core_event);
            // Capture the error before `encode_core_event` consumes the event,
            // so we can synthesize an SSE error frame if the OpenAI encoder
            // drops it (it returns `Err` for `CoreEvent::Error`).
            let leading_openai_error = if matches!(ctx.client_protocol, ClientProtocol::OpenAiChat) {
                match &core_event {
                    CoreEvent::Error { error } => Some(error.clone()),
                    _ => None,
                }
            } else {
                None
            };
            let client_events = encode_core_event(&mut encoder, core_event);
            if client_events.is_empty()
                && let Some(stream_error) = leading_openai_error
            {
                // If serialization failed there is nothing else to do for
                // this event; continue the replay loop.
                let Some(json_str) = openai_chat::stream_error_sse_payload(&stream_error) else {
                    continue;
                };
                if !ctx.emit_event(Event::default().data(json_str), &tx).await {
                    return;
                }
                // Emit the OpenAI `[DONE]` terminator after the error.
                if !ctx.emit_event(openai_done_event(), &tx).await {
                    return;
                }
                encoder.mark_finished();
                stream_succeeded = false;
                stream_errored = true;
                // Record the failure and emit a ResponseFailed event so
                // the in-band error is reflected in metrics/event-log,
                // mirroring the post-first-byte in-band error path. The
                // HTTP status is already committed as 200 (the error
                // crossed the boundary in-band), but the event still
                // classifies the failure for operators.
                ctx.stream_metrics.metrics.record_failure();
                let route_error = RouteError::ProviderDecode(stream_error.message().to_owned());
                emit_response_failed(
                    &ctx.event_bus,
                    &ctx.request_id,
                    Some(ctx.stream_metrics.provider_name.as_str()),
                    Some(&ctx.core.model),
                    &route_error,
                    ctx.stream_metrics.start,
                );
                continue;
            }
            for event in client_events {
                if !ctx.emit_event(event, &tx).await {
                    return;
                }
            }
        }

        // Skip the main consumption loop when the replay already terminated
        // the stream with an in-band error (leading CoreEvent::Error on the
        // OpenAI path). The error + [DONE] have already crossed the boundary;
        // reading more from the upstream would only append noise after the
        // terminal error (audit LOW, stream-ids-01).
        if !stream_errored {
        'outer: loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    // Client disconnected; abort upstream stream. This is a
                    // client-initiated cancellation, not an upstream failure:
                    // record it as a cancellation so it does not inflate the
                    // error-rate SLO (audit MEDIUM-5).
                    ctx.stream_metrics.metrics.record_client_cancel();
                    if !ctx.first_byte_sent {
                        ctx.send_pre_stream_error(
                            RouteError::Internal("client disconnected before first byte".to_owned()),
                        );
                    } else {
                        // Post-first-byte disconnect: emit a terminal
                        // `ClientCancelled` event so the lifecycle closes
                        // symmetrically with the pre-first-byte path (which
                        // emits via PreStreamError). Audit finding: buffering
                        // vs main-loop disconnect event inconsistency.
                        ctx.emit_client_cancelled();
                    }
                    return;
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            let frames = match ctx.sse_framer.push_chunk(&bytes) {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!(
                                        request_id = %ctx.request_id,
                                        error = %e,
                                        "SSE framing error in stream"
                                    );
                                    if !ctx.first_byte_sent {
                                        ctx.stream_metrics.metrics.record_failure();
                                        ctx.send_pre_stream_error(
                                            RouteError::ProviderDecode(
                                                format!("stream framing error: {e}"),
                                            ),
                                        );
                                    } else {
                                        let err = RouteError::ProviderDecode(format!(
                                            "stream framing error: {e}"
                                        ));
                                        emit_stream_error(
                                            &mut encoder,
                                            &tx,
                                            &ctx.client_protocol,
                                            PROVIDER_DECODE_CLIENT_MESSAGE,
                                        ).await;
                                        ctx.stream_metrics.metrics.record_failure();
                                        emit_response_failed(
                                            &ctx.event_bus,
                                            &ctx.request_id,
                                            Some(ctx.stream_metrics.provider_name.as_str()),
                                            Some(&ctx.core.model),
                                            &err,
                                            ctx.stream_metrics.start,
                                        );
                                    }
                                    stream_errored = true;
                                    break 'outer;
                                }
                            };

                            for frame in &frames {
                                let core_events = match ctx.provider_decoder.decode_frame(frame) {
                                    Ok(events) => events,
                                    Err(e) => {
                                        warn!(
                                            request_id = %ctx.request_id,
                                            error = %e,
                                            "provider decode error in stream"
                                        );
                                        if !ctx.first_byte_sent {
                                            ctx.stream_metrics.metrics.record_failure();
                                            ctx.send_pre_stream_error(
                                                RouteError::ProviderDecode(
                                                    format!("provider decode error: {e}"),
                                                ),
                                            );
                                        } else {
                                            let err = RouteError::ProviderDecode(
                                                format!("provider decode error: {e}"),
                                            );
                                            emit_stream_error(
                                                &mut encoder,
                                                &tx,
                                                &ctx.client_protocol,
                                                PROVIDER_DECODE_CLIENT_MESSAGE,
                                            ).await;
                                            ctx.stream_metrics.metrics.record_failure();
                                            emit_response_failed(
                                                &ctx.event_bus,
                                                &ctx.request_id,
                                                Some(ctx.stream_metrics.provider_name.as_str()),
                                                Some(&ctx.core.model),
                                                &err,
                                                ctx.stream_metrics.start,
                                            );
                                        }
                                        stream_errored = true;
                                        break 'outer;
                                    }
                                };

                                for core_event in core_events {
                                    capture_lifecycle(&mut ctx, &core_event);
                                    let client_events = encode_core_event(
                                        &mut encoder,
                                        core_event,
                                    );
                                    for event in client_events {
                                        if !ctx.emit_event(event, &tx).await {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(
                                request_id = %ctx.request_id,
                                error = %e,
                                "upstream stream error"
                            );
                            if !ctx.first_byte_sent {
                                ctx.stream_metrics.metrics.record_failure();
                                ctx.send_pre_stream_error(
                                    map_provider_error(&e, ctx.passthrough_auth),
                                );
                            } else {
                                // In-band stream error: run through the full
                                // sanitizer (key-redact + URL-redact + truncate)
                                // so a future error string carrying an API key
                                // is redacted, matching the `Api` HTTP path
                                // (audit GAP-LOW-2).
                                let sanitized = fully_sanitize_upstream_error(&e.to_string());
                                emit_stream_error(
                                    &mut encoder,
                                    &tx,
                                    &ctx.client_protocol,
                                    &sanitized,
                                ).await;
                                ctx.stream_metrics.metrics.record_failure();
                                // Classify the in-band error through
                                // `map_provider_error` (the same helper the
                                // non-stream and pre-stream paths use) so the
                                // ResponseFailed.http_status matches the
                                // non-stream classification: an upstream `Api`
                                // error preserves its real status (429/400/413
                                // pass through; 5xx -> 502), and an in-band
                                // reqwest timeout is attributed as an upstream
                                // timeout (504) rather than a generic 502
                                // (audit LOW, eventlog-inband-stream-httpstatus).
                                let route_error = map_provider_error(&e, ctx.passthrough_auth);
                                emit_response_failed(
                                    &ctx.event_bus,
                                    &ctx.request_id,
                                    Some(ctx.stream_metrics.provider_name.as_str()),
                                    Some(&ctx.core.model),
                                    &route_error,
                                    ctx.stream_metrics.start,
                                );
                            }
                            stream_errored = true;
                            break 'outer;
                        }
                        None => {
                            // Stream ended with no events at all.
                            // If first byte was never sent, this is a normal
                            // completion of an empty stream -- the handler will
                            // see the channel close and respond accordingly.
                            break 'outer;
                        }
                    }
                }
            }
        }
        } // end `if !stream_errored` guard around the main consumption loop

        // Only run finalization when the stream completed normally (not errored)
        // AND actually produced output (first_byte_sent). An empty / never-started
        // upstream stream must NOT run finalization: the provider decoder's
        // finish() would synthesize MessageStart+MessageStop, cross the first-byte
        // boundary, and commit HTTP 200. Skipping finalization lets such a stream
        // fall through to the PreStreamError -> 502 path below.
        if !stream_errored && ctx.first_byte_sent {
            // Finalize: call sse_framer.finish() first to flush any trailing partial
            // SSE frame that arrived without a terminating blank line.
            if !finalize_sse_framer(&mut ctx, &mut encoder, &tx).await {
                return;
            }

            // Then finalize: call provider decoder finish() to emit any remaining events.
            match ctx.provider_decoder.finish() {
                Ok(final_events) => {
                    if !emit_finalization_events(&mut ctx, &mut encoder, &tx, final_events).await {
                        return;
                    }
                }
                Err(e) => {
                    warn!(
                        request_id = %ctx.request_id,
                        error = %e,
                        "provider decoder finish error"
                    );
                }
            }

            // Emit any remaining client encoder events (synthetic terminal if needed).
            match encoder.finish() {
                Ok(final_encoded_events) => {
                    if !emit_encoder_finish(&mut ctx, &tx, final_encoded_events).await {
                        return;
                    }
                    stream_succeeded = true;
                }
                Err(e) => {
                    warn!(
                        request_id = %ctx.request_id,
                        error = %e,
                        "client encoder finish error"
                    );
                    ctx.stream_metrics.metrics.record_failure();
                }
            }
        }

        // Record metrics after stream completes.
        if stream_succeeded {
            ctx.stream_metrics.metrics.record_success(
                &ctx.stream_metrics.provider_name,
                &ctx.stream_metrics.upstream_model,
                ctx.stream_metrics.start.elapsed(),
            );
            // Emit the stream ResponseCompleted event with accumulated lifecycle
            // data (usage, cost, stop reason, upstream message id).
            let usage = ctx
                .pending_usage
                .clone()
                .unwrap_or_else(Usage::synthetic_zero);
            // Only attach a cost when the usage was genuinely reported by the
            // provider. Synthetic/Unknown usage (the `unwrap_or_else` zero-fill
            // when no `UsageDelta` arrived) would otherwise produce a
            // Some(zero-Cost) indistinguishable from a truly free request;
            // leaving cost None for unprovenanced usage keeps the event honest
            // (audit INFO, cost-on-synthetic-zero). Mirrors handle_core_once.
            let cost = if usage.provenance == UsageProvenance::ProviderReported {
                ctx.pricing.as_ref().map(|p| compute_cost(&usage, p))
            } else {
                None
            };
            ctx.event_bus
                .emit(&ProxyEvent::ResponseCompleted(ResponseCompleted {
                    request_id: ctx.request_id.clone(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    provider: ctx.stream_metrics.provider_name.clone(),
                    upstream_message_id: ctx.upstream_message_id.clone(),
                    model: ctx.core.model.clone(),
                    usage,
                    cost,
                    stop_reason: ctx.stop_reason.clone().unwrap_or(StopReason::EndTurn),
                    latency_ms: ctx
                        .stream_metrics
                        .start
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                }));
        }

        // If the stream ended without ever sending a first byte (empty stream
        // with no errors), send a PreStreamError with a descriptive message.
        // The handler will return this as a 502 Bad Gateway to the client.
        // This is preferable to silently dropping the channel (which produces
        // a misleading "stream task panicked" error).
        if !ctx.first_byte_sent
            && let Some(tx) = ctx.first_byte_tx.take()
            && tx
                .send(FirstByteResult::PreStreamError(RouteError::ProviderDecode(
                    "upstream returned an empty stream with no events".to_owned(),
                )))
                .is_err()
        {
            tracing::debug!("client disconnected before the empty-stream error could be reported");
        }
    }
        .instrument(stream_span),
    );

    // Supervise the stream task so a panic *after* the first byte is not
    // swallowed. Pre-first-byte panics surface to the client through the
    // `first_byte_rx` RecvError path (mapped to RouteError::Internal in the
    // handler); post-first-byte panics drop `tx` and silently end the stream
    // with no event/metric. This supervisor closes that observability gap:
    // on a JoinError it emits a ResponseFailed event and records a failure so
    // operators can diagnose the panic (audit LOW, detached spawn). The stream
    // task itself is not blocked -- it terminates independently and the
    // supervisor merely observes its exit.
    tokio::spawn(async move {
        match stream_task.await {
            Ok(()) => {}
            Err(join_error) => {
                let panicked = join_error.is_panic();
                let err = RouteError::Internal(format!(
                    "stream task {}",
                    if panicked {
                        "panicked"
                    } else {
                        "was cancelled"
                    }
                ));
                warn!(
                    request_id = %supervise_request_id,
                    error = %join_error,
                    panicked,
                    "detached stream task did not complete cleanly"
                );
                // Only record/emit if the handler's `first_byte_rx` RecvError
                // path has not already done so for this same pre-first-byte
                // panic. The shared `failure_recorded` flag is claimed by the
                // first watcher to run; the second skips, so a pre-first-byte
                // panic produces exactly one `ResponseFailed` + one failure
                // metric (audit finding: stream-task panic double-ResponseFailed).
                // Post-first-byte panics are only ever observed here (the handler
                // is no longer awaiting `first_byte_rx`), so the flag is still
                // false and the supervisor emits.
                if !failure_recorded.swap(true, Ordering::SeqCst) {
                    supervise_metrics.record_failure();
                    emit_response_failed(
                        &supervise_event_bus,
                        &supervise_request_id,
                        Some(supervise_provider.as_str()),
                        Some(&supervise_model),
                        &err,
                        supervise_start,
                    );
                }
            }
        }
    });

    // Wrap the receiver so that dropping it cancels the spawned task.
    let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let cancel_guard = cancel.drop_guard();
    rx_stream
        .map(move |item| {
            let _guard = &cancel_guard;
            item
        })
        .boxed()
}

/// Extract the upstream message ID from the first `MessageStart` event in a
/// batch, if any carries one.
///
/// Used to seed the client stream encoder with the provider's real message ID
/// instead of a synthetic one. Only `MessageStart { id: Some(..) }` contributes;
/// earlier events in the batch (e.g. `Ping`) are skipped, so a
/// `Ping`-then-`MessageStart` opening still surfaces the real id.
fn extract_upstream_message_id(events: &[CoreEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        CoreEvent::MessageStart { id: Some(id), .. } => Some(id.clone()),
        _ => None,
    })
}

/// Encode a single [`CoreEvent`] using the appropriate client stream encoder.
///
/// # Silently dropped events
///
/// If encoding fails (e.g. the encoder encounters an invalid state), the event
/// is logged at warn level and silently dropped. This prevents a single
/// malformed event from terminating the entire SSE stream. The trade-off is
/// that clients may miss events without explicit notification. This is
/// acceptable because:
/// 1. Encoding failures indicate a proxy bug, not a transient client issue.
/// 2. The stream will still terminate normally (with `finish()`) rather than
///    abruptly mid-frame.
/// 3. The `warn!` log ensures operators can diagnose the root cause.
fn encode_core_event(client_encoder: &mut ClientStreamEncoder, event: CoreEvent) -> Vec<Event> {
    match client_encoder.encode_event(event) {
        Ok(encoded_events) => encoded_events
            .into_iter()
            .map(client_event_to_sse)
            .collect(),
        Err(e) => {
            warn!(error = %e, "client stream encode error");
            Vec::new()
        }
    }
}

/// Emit an error event into the stream.
///
/// For OpenAI Chat, the StreamEncoder returns `Err` for `CoreEvent::Error`.
/// Rather than silently dropping the error, we construct a raw SSE error
/// event directly so the client sees an error indication before `[DONE]`.
///
/// After the error event, the encoder's `finished` flag is set to `true` so
/// that any subsequent `finish()` call returns an empty vec. The error event
/// itself is the terminal event -- no synthetic message_delta/message_stop
/// pair should follow an error.
async fn emit_stream_error(
    client_encoder: &mut ClientStreamEncoder,
    tx: &tokio::sync::mpsc::Sender<Event>,
    client_protocol: &ClientProtocol,
    message: &str,
) {
    use llm_proxy_protocol::core::{CoreStreamError, CoreStreamErrorKind};

    let error_event = CoreEvent::Error {
        error: CoreStreamError::new(CoreStreamErrorKind::Upstream, message.to_owned()),
    };

    let events = encode_core_event(client_encoder, error_event);
    if events.is_empty() && matches!(client_protocol, ClientProtocol::OpenAiChat) {
        // The OpenAI StreamEncoder returns Err for CoreEvent::Error, which
        // causes encode_core_event to produce an empty Vec. Construct an SSE
        // error event using the same typed structs as the HTTP error path so
        // both paths stay consistent at compile time.
        //
        // Use "server_error" as the error type to match the HTTP 500 path's
        // convention in openai_error_response, since in-band stream errors are
        // always upstream/internal issues.
        if let Some(json_str) = openai_stream_error_json_with_type(message, "server_error")
            && tx.send(Event::default().data(json_str)).await.is_err()
        {
            return;
        }
    } else {
        for event in events {
            if tx.send(event).await.is_err() {
                return;
            }
        }
    }

    // For OpenAI Chat, emit the [DONE] terminator after an error event.
    if matches!(client_protocol, ClientProtocol::OpenAiChat)
        && tx.send(openai_done_event()).await.is_err()
    {
        return;
    }

    // Mark the encoder as finished so that subsequent finish() calls return
    // empty events. The error event itself is the terminal event -- we do NOT
    // emit synthetic message_delta/message_stop after an error.
    client_encoder.mark_finished();
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map a [`ProviderError`](llm_proxy_provider::error::ProviderError) to a
/// [`RouteError`].
///
/// Upstream error bodies are sanitized before being forwarded to clients:
/// - `Api` variants: already sanitized at construction time.
/// - Non-`Api` variants (Http, Serialize, etc.): run through
///   [`fully_sanitize_upstream_error`], which applies the canonical
///   key-redacting sanitizer (via [`ProviderError::api`]) followed by URL
///   redaction and truncation, so neither API keys nor upstream hostnames/URL
///   paths leak to the client (audit GAP-LOW-2).
pub(crate) fn map_provider_error(
    e: &llm_proxy_provider::error::ProviderError,
    passthrough_auth: bool,
) -> RouteError {
    // Whose credentials authenticated with the upstream: under passthrough_auth
    // the client's own token is forwarded, so auth/permission failures are
    // client-owned; otherwise they are operator-owned. This drives whether
    // 401/403 pass through or collapse to 502 in extract_error_fields
    // (audit LOW, passthrough-auth-401-collapse).
    let auth_owner = if passthrough_auth {
        AuthOwner::Client
    } else {
        AuthOwner::Operator
    };
    match e {
        llm_proxy_provider::error::ProviderError::Api { status, body } => {
            let status_code = StatusCode::from_u16(*status).unwrap_or_else(|_| {
                warn!(
                    status = *status,
                    "invalid HTTP status from provider; mapping to 502"
                );
                StatusCode::BAD_GATEWAY
            });
            RouteError::Upstream {
                status: status_code,
                body: body.clone(),
                auth_owner,
            }
        }
        _ => {
            // Check for reqwest timeout specifically so we can return 504
            // instead of the generic 502 Bad Gateway.
            if e.is_timeout() {
                let sanitized = fully_sanitize_upstream_error(&e.to_string());
                return RouteError::UpstreamTimeout(sanitized);
            }
            // Sanitize non-Api error messages through the full sanitizer
            // (key-redact + URL-redact + truncate) so a future error string
            // carrying an API key is redacted, matching the `Api` branch above
            // (audit GAP-LOW-2).
            let sanitized = fully_sanitize_upstream_error(&e.to_string());
            RouteError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: sanitized,
                auth_owner,
            }
        }
    }
}

/// Maximum length for sanitized upstream error bodies before passing to
/// `truncate_error_body()` in the error response encoder.
const MAX_SANITIZE_LEN: usize = 512;
const SANITIZE_SUFFIX: &str = "...[truncated]";

/// Compiled regex for URL redaction, initialized once via `LazyLock`.
static URL_REDACT_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"https?://\S+").expect("valid URL redaction regex")
});

/// Sanitize non-Api upstream error bodies to prevent information leakage.
///
/// Non-Api errors (Http, Serialize, Utf8, etc.) are converted via
/// `e.to_string()`, which for reqwest errors can contain full URLs including
/// hostnames and paths. This function replaces such details with generic
/// messages while preserving enough context for debugging.
pub(super) fn sanitize_upstream_error_body(msg: &str) -> String {
    // Truncate to MAX_SANITIZE_LEN as a safety net. The suffix is included
    // within this budget. The actual error body in RouteError::Upstream is
    // further truncated by truncate_error_body() in the error response encoder.
    let truncated = truncate_with_suffix(msg, MAX_SANITIZE_LEN, SANITIZE_SUFFIX);

    // Redact URL-like patterns that may contain hostnames/paths.
    // Matches http:// or https:// followed by any non-whitespace chars.
    URL_REDACT_REGEX
        .replace_all(&truncated, "[url-redacted]")
        .into_owned()
}

/// Sanitize a raw non-`Api` upstream error string through the **full** sanitizer
/// pipeline: the canonical key-redacting sanitizer plus URL redaction and
/// truncation.
///
/// This is the defense-in-depth counterpart to `sanitize_upstream_error_body`:
/// that function only redacts URLs and truncates, so a future error string that
/// happens to carry an API key (e.g. a provider echoing a `Bearer` token or an
/// `sk-...` value in a transport message) would reach the client un-redacted.
/// Routing the divergent error paths (streaming in-band errors and non-`Api`
/// HTTP errors) through this function keeps them consistent with the `Api`
/// branch, which is already key-redacted at construction time
/// (`ProviderError::api` → `sanitize_api_error_body`).
///
/// # Implementation
///
/// The canonical key-redacting sanitizer lives in
/// `llm_proxy_provider::error::sanitize_api_error_body` and is `pub(crate)` to
/// the provider crate, so it cannot be called directly. Its only public entry
/// point is the [`ProviderError::api`] constructor, whose doc comment states it
/// "encapsulates the sanitization call so callers never need to remember to
/// call the internal `sanitize_api_error_body` function manually." We therefore
/// round-trip the message through that constructor (with a sentinel status that
/// is never surfaced — only the `body` is read back) to obtain a key-redacted,
/// truncated string, then run the result through [`sanitize_upstream_error_body`]
/// for URL redaction. Both truncation passes are idempotent (the second is a
/// no-op because the body is already within `MAX_SANITIZE_LEN`).
pub(super) fn fully_sanitize_upstream_error(msg: &str) -> String {
    // Apply the canonical key-redaction + truncation via the public `api`
    // constructor. The status code is a sentinel: only the sanitized `body`
    // field is extracted below, and this function never produces a `RouteError`
    // whose status depends on it.
    let key_redacted = match llm_proxy_provider::error::ProviderError::api(0, msg.to_owned()) {
        llm_proxy_provider::error::ProviderError::Api { body, .. } => body,
        // `api()` is guaranteed to construct the `Api` variant, but
        // `ProviderError` is `#[non_exhaustive]`, so a catch-all keeps the
        // match exhaustive against future variants without silently changing
        // behaviour. Fall back to the URL+truncate sanitizer.
        _ => return sanitize_upstream_error_body(msg),
    };
    // Layer URL redaction on top (truncation is idempotent here).
    sanitize_upstream_error_body(&key_redacted)
}

/// Map a [`ProtocolError`](llm_proxy_protocol::client::ProtocolError) to a
/// [`RouteError`].
///
/// Per the `ProtocolError` doc comments:
/// - `InvalidRequest` -> 400 Bad Request (bad client input)
/// - `Decode` -> 400 Bad Request (malformed client input)
/// - `Encode` -> 500 Internal Server Error
/// - `EncodeSkippable` -> mapped as non-fatal (skipped), so 500 if it
///   reaches this path.
pub(crate) fn protocol_error_to_route(e: llm_proxy_protocol::client::ProtocolError) -> RouteError {
    match e {
        llm_proxy_protocol::client::ProtocolError::InvalidRequest(msg) => {
            RouteError::InvalidRequest(msg)
        }
        llm_proxy_protocol::client::ProtocolError::Decode(msg) => {
            // Per ProtocolError doc: Decode -> 400 Bad Request (malformed client input).
            RouteError::InvalidRequest(msg)
        }
        other => RouteError::Internal(other.to_string()),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_cost_multiplies_usage_by_per_token_prices() {
        use llm_proxy_core::ModelPricing;
        use llm_proxy_protocol::core::Usage;

        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            reasoning_tokens: Some(50),
            cache_creation_input_tokens: Some(200),
            cache_read_input_tokens: None,
            ..Usage::default()
        };
        let pricing = ModelPricing {
            input: "0.0000015".parse().unwrap(),
            output: "0.000003".parse().unwrap(),
            cache_creation: "0.000001875".parse().unwrap(),
            cache_read: "0.00000015".parse().unwrap(),
            reasoning: "0.000003".parse().unwrap(),
        };
        let cost = compute_cost(&usage, &pricing);
        // input:         1000 * 0.0000015    = 0.0015
        // output:         500 * 0.000003     = 0.0015
        // cache_creation: 200 * 0.000001875  = 0.000375
        // cache_read:     None -> 0
        // reasoning:       50 * 0.000003     = 0.00015
        // total = 0.0015 + 0.0015 + 0.000375 + 0 + 0.00015 = 0.003525
        // `.normalize().to_string()` strips trailing zeros so the expected
        // strings are scale-independent.
        assert_eq!(cost.input.normalize().to_string(), "0.0015");
        assert_eq!(cost.output.normalize().to_string(), "0.0015");
        assert_eq!(cost.cache_creation.normalize().to_string(), "0.000375");
        assert_eq!(cost.cache_read.normalize().to_string(), "0");
        assert_eq!(cost.reasoning.normalize().to_string(), "0.00015");
        assert_eq!(cost.total.normalize().to_string(), "0.003525");
    }

    #[test]
    fn compute_cost_rounds_to_microcent() {
        // Per the plan's locked decision 5, cost is rounded to the nearest
        // microcent (6 dp) on output. A price that would otherwise carry more
        // than 6 dp (0.000001875 * 3 tokens = 0.000005625) is rounded.
        use llm_proxy_core::ModelPricing;
        use llm_proxy_protocol::core::Usage;
        let usage = Usage {
            input_tokens: 3,
            output_tokens: 0,
            ..Usage::default()
        };
        let pricing = ModelPricing {
            input: "0.000001875".parse().unwrap(),
            ..ModelPricing::default()
        };
        let cost = compute_cost(&usage, &pricing);
        // 3 * 0.000001875 = 0.000005625 -> rounded to 6 dp -> 0.000006 (half up).
        assert_eq!(cost.input.normalize().to_string(), "0.000006");
        // total == input (the only nonzero component).
        assert_eq!(cost.total.normalize().to_string(), "0.000006");
    }

    #[test]
    fn compute_cost_saturates_on_overflow_and_never_panics() {
        // `rust_decimal`'s `*`/`+` panic on overflow; compute_cost must use
        // checked arithmetic so a pathological (count, price) pair saturates to
        // Decimal::MAX instead of crashing the request. The validation bound
        // prevents such prices from reaching compute_cost via config, but this
        // guards the function itself against any caller.
        use llm_proxy_core::ModelPricing;
        use llm_proxy_protocol::core::Usage;
        let usage = Usage {
            input_tokens: i32::MAX,
            output_tokens: i32::MAX,
            reasoning_tokens: Some(i32::MAX),
            cache_creation_input_tokens: Some(i32::MAX),
            cache_read_input_tokens: Some(i32::MAX),
            ..Usage::default()
        };
        let pricing = ModelPricing {
            input: "999999999999999999999".parse().unwrap(),
            output: "999999999999999999999".parse().unwrap(),
            cache_creation: "999999999999999999999".parse().unwrap(),
            cache_read: "999999999999999999999".parse().unwrap(),
            reasoning: "999999999999999999999".parse().unwrap(),
        };
        // Must not panic.
        let cost = compute_cost(&usage, &pricing);
        assert_eq!(cost.input, Decimal::MAX);
        assert_eq!(cost.total, Decimal::MAX);
    }

    /// Regression guard for the 502-collapse secret/schema leak (Cluster A) and
    /// the value-oriented source-guard gap (finding #8). The storage crate's
    /// source-guard only checks that secret-bearing KEY NAMES never appear as
    /// JSON keys; it does not check that a secret VALUE is absent from the
    /// `message` string. This test routes an upstream error carrying a
    /// realistic API-key-shaped value through `emit_response_failed` and asserts
    /// the secret appears NOWHERE in the serialized event JSON.
    #[test]
    fn emit_response_failed_redacts_secret_values_from_message() {
        use llm_proxy_storage::RecordingBus;
        let bus = Arc::new(RecordingBus::default());
        let bus_dyn: Arc<dyn EventBus> = Arc::clone(&bus) as Arc<dyn EventBus>;
        let error = RouteError::Upstream {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "auth failed for key sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcd"
                .to_owned(),
            auth_owner: AuthOwner::Operator,
        };
        emit_response_failed(
            &bus_dyn,
            "req-1",
            Some("prov"),
            None,
            &error,
            Instant::now(),
        );
        let recorded = bus
            .events
            .lock()
            .expect("bus lock")
            .iter()
            .find_map(|e| match e {
                ProxyEvent::ResponseFailed(f) => Some(f.clone()),
                _ => None,
            })
            .expect("a ResponseFailed was emitted");
        // error_kind is the RouteError variant name (Cluster C guard).
        assert_eq!(recorded.error_kind, "Upstream");
        // The secret value must not appear in the message...
        assert!(
            !recorded.message.contains("sk-ant-api03-"),
            "secret prefix leaked into message: {}",
            recorded.message
        );
        assert!(
            !recorded.message.contains("ABCDEFGHIJKLMNOP"),
            "key material leaked into message: {}",
            recorded.message
        );
        // ...nor anywhere in the serialized event JSON (the storage key-name
        // guard would miss this; this is the value-oriented guard).
        let json = serde_json::to_string(&ProxyEvent::ResponseFailed(recorded)).unwrap();
        assert!(
            !json.contains("sk-ant-api03-") && !json.contains("ABCDEFGHIJKLMNOP"),
            "secret value leaked into event JSON: {json}"
        );
    }

    /// The passthrough-auth Authorization scheme is case-insensitive (RFC 7235):
    /// "Bearer", "bearer", "BEARER", "bEaReR" must all be accepted. Guards the
    /// Cluster F fix that replaced a two-casing prefix match.
    #[test]
    fn extract_inbound_auth_bearer_scheme_is_case_insensitive() {
        use secrecy::ExposeSecret;
        fn extracted_token(auth_header: &str) -> Option<String> {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, auth_header.parse().unwrap());
            extract_inbound_auth(&headers).map(|s| s.expose_secret().to_owned())
        }
        assert_eq!(extracted_token("Bearer abc123").as_deref(), Some("abc123"));
        assert_eq!(extracted_token("bearer abc123").as_deref(), Some("abc123"));
        assert_eq!(extracted_token("BEARER abc123").as_deref(), Some("abc123"));
        assert_eq!(extracted_token("BeArEr abc123").as_deref(), Some("abc123"));
        // The token itself is trimmed; surrounding spaces do not leak in.
        assert_eq!(
            extracted_token("Bearer   abc123   ").as_deref(),
            Some("abc123")
        );
        // A non-Bearer scheme is not extracted via Authorization.
        assert_eq!(extracted_token("Basic dXNlcjpwYXNz"), None);
        // An empty token after the scheme yields None (falls through to the
        // x-api-key / x-goog-api-key headers, which are absent here).
        assert_eq!(extracted_token("Bearer "), None);
    }

    #[test]
    fn body_hash_is_stable_and_distinguishes_requests() {
        // `body_hash` is computed over the raw client body bytes, matching the
        // `/v1/messages/count_tokens` route, so identical raw bodies hash
        // equally and distinct bodies differ. Two bodies that normalize to the
        // same CoreRequest but differ in whitespace/byte order hash differently
        // because the correlation key tracks the raw wire payload.
        let a1 = br#"{"model":"gpt-4o","messages":[]}"#;
        let a2 = br#"{"model":"gpt-4o","messages":[]}"#;
        let b = br#"{"model":"gpt-3.5-turbo","messages":[]}"#;
        assert_eq!(
            body_hash_of_bytes(a1),
            body_hash_of_bytes(a2),
            "identical raw bodies must produce the same hash"
        );
        assert_ne!(
            body_hash_of_bytes(a1),
            body_hash_of_bytes(b),
            "different raw bodies must produce different hashes"
        );
        // Whitespace-sensitive (raw bytes, not normalized): the same JSON with
        // a space after the colon hashes differently.
        let spaced = br#"{"model": "gpt-4o","messages":[]}"#;
        assert_ne!(
            body_hash_of_bytes(a1),
            body_hash_of_bytes(spaced),
            "raw-body hash must be sensitive to whitespace (it is over the wire bytes)"
        );
    }

    fn state_with_operational_config(
        rate_limit_rpm: u32,
        trust_forwarded_headers: bool,
    ) -> AppState {
        let app_config = llm_proxy_core::AppConfig {
            server: llm_proxy_core::ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: std::time::Duration::from_secs(60),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                rate_limit_rpm,
                trust_forwarded_headers,
                dedup_window: std::time::Duration::ZERO,
                server_name: "test".to_owned(),
                log_format: Default::default(),
            },
            auth: Default::default(),
            storage: None,
        };
        let providers =
            llm_proxy_core::ProviderRegistry::from_providers(vec![]).expect("empty registry");

        AppState::new(
            app_config,
            providers,
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::new(),
            crate::state::BuildInfo {
                name: "test",
                version: "0.0.0",
                target: "test",
                git_sha: "test",
            },
        )
    }

    // -- protocol_error_to_route ------------------------------------------------

    #[test]
    fn protocol_error_invalid_request_maps_to_route_invalid_request() {
        let err = llm_proxy_protocol::client::ProtocolError::InvalidRequest("bad".into());
        let route_err = protocol_error_to_route(err);
        assert!(matches!(route_err, RouteError::InvalidRequest(_)));
    }

    #[test]
    fn protocol_error_decode_maps_to_route_invalid_request() {
        let err = llm_proxy_protocol::client::ProtocolError::Decode("bad".into());
        let route_err = protocol_error_to_route(err);
        match route_err {
            RouteError::InvalidRequest(msg) => assert!(msg.contains("bad")),
            other => panic!("expected InvalidRequest, got: {:?}", other),
        }
    }

    #[test]
    fn protocol_error_encode_maps_to_route_internal() {
        let err = llm_proxy_protocol::client::ProtocolError::Encode("bad".into());
        let route_err = protocol_error_to_route(err);
        assert!(matches!(route_err, RouteError::Internal(_)));
    }

    #[test]
    fn direct_loopback_bypasses_rate_limit() {
        let state = state_with_operational_config(1, false);
        let headers = HeaderMap::new();
        let meta = test_meta(
            crate::middleware::CallerId::anonymous(),
            Some("127.0.0.1:12345"),
        );

        prepare_request(&state, &meta, &headers, b"one", "/test")
            .expect("request under the rate limit should be admitted");
        prepare_request(&state, &meta, &headers, b"two", "/test")
            .expect("request under the rate limit should be admitted");
    }

    /// Build a `RequestMeta` for direct `prepare_request` calls.
    fn test_meta(
        caller: crate::middleware::CallerId,
        peer: Option<&str>,
    ) -> crate::middleware::RequestMeta {
        crate::middleware::RequestMeta {
            request_id: crate::middleware::RequestId("req-test".to_owned()),
            caller,
            connect_info: peer.map(|addr| addr.parse().unwrap()),
        }
    }

    #[test]
    fn untrusted_forwarded_headers_cannot_bypass_rate_limit() {
        let state = state_with_operational_config(1, false);
        let meta = test_meta(
            crate::middleware::CallerId::anonymous(),
            Some("198.51.100.10:12345"),
        );
        let mut first_headers = HeaderMap::new();
        first_headers.insert("x-forwarded-for", "203.0.113.1".parse().unwrap());
        let mut second_headers = HeaderMap::new();
        second_headers.insert("x-forwarded-for", "203.0.113.2".parse().unwrap());

        prepare_request(&state, &meta, &first_headers, b"one", "/test")
            .expect("request under the rate limit should be admitted");
        assert!(matches!(
            prepare_request(&state, &meta, &second_headers, b"two", "/test"),
            Err(RouteError::RateLimited)
        ));
    }

    #[test]
    fn authenticated_caller_is_limited_by_key_id_across_addresses() {
        // An authenticated caller's bucket is the key id, not the peer IP: a
        // second request carrying the same key from a different address
        // shares the budget (1 rpm here), so it is rejected.
        let state = state_with_operational_config(1, false);
        let caller = crate::middleware::CallerId::authenticated("laptop");
        let first_meta = test_meta(caller.clone(), Some("198.51.100.10:12345"));
        let second_meta = test_meta(caller, Some("198.51.100.11:12345"));
        let headers = HeaderMap::new();

        prepare_request(&state, &first_meta, &headers, b"one", "/test")
            .expect("first request under the rate limit should be admitted");
        assert!(matches!(
            prepare_request(&state, &second_meta, &headers, b"two", "/test"),
            Err(RouteError::RateLimited)
        ));
    }

    #[test]
    fn authenticated_loopback_caller_is_still_rate_limited() {
        // The loopback bypass exists for anonymous local clients (today's
        // behavior under auth = "none"); an authenticated caller pays the
        // limiter even from 127.0.0.1.
        let state = state_with_operational_config(1, false);
        let meta = test_meta(
            crate::middleware::CallerId::authenticated("laptop"),
            Some("127.0.0.1:12345"),
        );
        let headers = HeaderMap::new();

        prepare_request(&state, &meta, &headers, b"one", "/test")
            .expect("first request under the rate limit should be admitted");
        assert!(matches!(
            prepare_request(&state, &meta, &headers, b"two", "/test"),
            Err(RouteError::RateLimited)
        ));
    }

    #[test]
    fn request_context_carries_caller_attribution() {
        let state = state_with_operational_config(100, false);
        let headers = HeaderMap::new();
        let ctx = prepare_request(
            &state,
            &test_meta(crate::middleware::CallerId::anonymous(), None),
            &headers,
            b"",
            "/test",
        )
        .expect("request admitted");
        assert_eq!(ctx.caller, "anonymous");

        let ctx = prepare_request(
            &state,
            &test_meta(crate::middleware::CallerId::authenticated("laptop"), None),
            &headers,
            b"",
            "/test",
        )
        .expect("request admitted");
        assert_eq!(ctx.caller, "laptop");
    }

    // -- map_provider_error -----------------------------------------------------

    #[test]
    fn provider_route_error_maps_unsupported_route_without_message_parsing() {
        let error = ProviderRouteResolutionError::UnsupportedRoute {
            provider: "example".to_owned(),
            route: "messages",
        };

        let route_error = map_provider_route_error(error);

        assert!(matches!(route_error, RouteError::UnsupportedRoute(_)));
    }

    #[test]
    fn provider_api_error_maps_to_upstream() {
        let err = llm_proxy_provider::error::ProviderError::api(500, "internal error".into());
        let route_err = map_provider_error(&err, false);
        match route_err {
            RouteError::Upstream { status, .. } => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            }
            other => panic!("expected Upstream, got: {:?}", other),
        }
    }

    #[test]
    fn provider_http_error_maps_to_upstream_502() {
        let err = llm_proxy_provider::error::ProviderError::api(502, "bad gateway".into());
        let route_err = map_provider_error(&err, false);
        match route_err {
            RouteError::Upstream { status, body, .. } => {
                assert_eq!(status, StatusCode::BAD_GATEWAY);
                assert!(body.contains("bad gateway"));
            }
            other => panic!("expected Upstream, got: {:?}", other),
        }
    }

    #[test]
    fn provider_api_error_marks_client_owner_under_passthrough() {
        // passthrough_auth=true => auth_owner is Client so 401/403 pass through.
        let err = llm_proxy_provider::error::ProviderError::api(401, "unauthorized".into());
        let route_err = map_provider_error(&err, true);
        match route_err {
            RouteError::Upstream {
                status, auth_owner, ..
            } => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(auth_owner, AuthOwner::Client);
            }
            other => panic!("expected Upstream, got: {:?}", other),
        }
    }

    #[test]
    fn provider_api_error_marks_operator_owner_by_default() {
        // passthrough_auth=false => auth_owner is Operator so 401 collapses to 502.
        let err = llm_proxy_provider::error::ProviderError::api(401, "unauthorized".into());
        let route_err = map_provider_error(&err, false);
        match route_err {
            RouteError::Upstream {
                status, auth_owner, ..
            } => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(auth_owner, AuthOwner::Operator);
            }
            other => panic!("expected Upstream, got: {:?}", other),
        }
    }

    // -- resolve_target with unknown provider -----------------------------------

    #[tokio::test]
    async fn resolve_target_unknown_provider_returns_unknown_provider() {
        use llm_proxy_core::AppConfig;
        use llm_proxy_core::ServerConfig;
        use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};

        let app_config = AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: std::time::Duration::from_secs(60),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "test".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: std::time::Duration::from_millis(500),
                log_format: Default::default(),
            },
            auth: Default::default(),
            storage: None,
        };

        let providers =
            llm_proxy_core::ProviderRegistry::from_providers(vec![]).expect("empty registry");

        let state = AppState::new(
            app_config,
            providers,
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            crate::state::BuildInfo {
                name: "test",
                version: "0.0.0",
                target: "test",
                git_sha: "test",
            },
        );

        let core = CoreRequest {
            model: llm_proxy_protocol::core::ModelRef {
                requested: "gpt-4o".to_owned(),
                upstream: None,
            },
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            sampling: Default::default(),
            stream: false,
            metadata: Default::default(),
            provider_hints: Default::default(),
        };

        let result = resolve_target(
            &state,
            "nonexistent",
            ProviderRouteKind::ChatCompletions,
            &core,
            None,
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            RouteError::UnknownProvider(name) => assert_eq!(name, "nonexistent"),
            other => panic!("expected UnknownProvider, got: {:?}", other),
        }
    }

    // -- resolve_target with empty registry returns unknown provider ------------

    #[tokio::test]
    async fn resolve_target_empty_registry_returns_unknown_provider() {
        use llm_proxy_core::AppConfig;
        use llm_proxy_core::ServerConfig;
        use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};

        let app_config = AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: std::time::Duration::from_secs(60),
                shutdown_timeout: Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "test".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: std::time::Duration::from_millis(500),
                log_format: Default::default(),
            },
            auth: Default::default(),
            storage: None,
        };

        let providers =
            llm_proxy_core::ProviderRegistry::from_providers(vec![]).expect("empty registry");

        let state = AppState::new(
            app_config,
            providers,
            ProviderAdapterRegistry::builtin(),
            ProxyClient::new(),
            crate::state::BuildInfo {
                name: "test",
                version: "0.0.0",
                target: "test",
                git_sha: "test",
            },
        );

        let core = CoreRequest {
            model: llm_proxy_protocol::core::ModelRef {
                requested: "some-model".to_owned(),
                upstream: None,
            },
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            sampling: Default::default(),
            stream: false,
            metadata: Default::default(),
            provider_hints: Default::default(),
        };

        let result = resolve_target(
            &state,
            "no-such-provider",
            ProviderRouteKind::Messages,
            &core,
            None,
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            RouteError::UnknownProvider(name) => assert_eq!(name, "no-such-provider"),
            other => panic!("expected UnknownProvider, got: {:?}", other),
        }
    }

    // -- encode_core_event smoke test -------------------------------------------

    #[test]
    fn encode_core_event_ping_produces_event() {
        use llm_proxy_protocol::core::CoreEvent;
        let mut encoder = ClientStreamEncoder::Anthropic(AnthropicStreamEncoder::new(
            "msg_test".to_owned(),
            "test-model".to_owned(),
        ));
        let events = encode_core_event(&mut encoder, CoreEvent::Ping);
        // Ping may or may not produce output depending on the encoder impl.
        // The important thing is it doesn't panic.
        let _ = events;
    }

    // -- sanitize_upstream_error_body tests ------------------------------------

    #[test]
    fn sanitize_removes_urls() {
        let msg =
            "request failed: connection refused to https://api.openai.com/v1/chat/completions";
        let sanitized = sanitize_upstream_error_body(msg);
        assert!(
            !sanitized.contains("api.openai.com"),
            "URL should be redacted"
        );
        assert!(
            sanitized.contains("[url-redacted]"),
            "should contain redacted placeholder"
        );
    }

    #[test]
    fn sanitize_truncates_long_messages() {
        let msg = "x".repeat(600);
        let sanitized = sanitize_upstream_error_body(&msg);
        assert!(sanitized.ends_with("...[truncated]"));
    }

    #[test]
    fn sanitize_preserves_short_messages_without_urls() {
        let msg = "connection reset by peer";
        let sanitized = sanitize_upstream_error_body(msg);
        assert_eq!(sanitized, msg);
    }

    // -- fully_sanitize_upstream_error tests (audit GAP-LOW-2) ------------------
    //
    // The divergent error paths (streaming in-band, non-Api timeout/generic)
    // used to run only `sanitize_upstream_error_body` (URL-redact + truncate),
    // which never strips API-key patterns. `fully_sanitize_upstream_error`
    // routes them through the canonical key-redacting sanitizer as well.

    #[test]
    fn fully_sanitize_redacts_api_key_patterns() {
        // A long sk-ant-... value (40 trailing token chars) is a real key shape.
        let msg =
            "upstream auth failed for key sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcd";
        let sanitized = fully_sanitize_upstream_error(msg);
        assert!(
            !sanitized.contains("sk-ant-api03-"),
            "API key prefix must be redacted, got: {sanitized}"
        );
        assert!(
            !sanitized.contains("ABCDEFGHIJKLMNOP"),
            "key material must be redacted, got: {sanitized}"
        );
        assert!(
            sanitized.contains("***"),
            "key should be replaced with the redaction marker"
        );
    }

    #[test]
    fn fully_sanitize_redacts_urls() {
        // URL redaction (from sanitize_upstream_error_body) must still apply.
        let msg = "connection refused to https://api.openai.com/v1/chat/completions";
        let sanitized = fully_sanitize_upstream_error(msg);
        assert!(
            !sanitized.contains("api.openai.com"),
            "URL host must be redacted, got: {sanitized}"
        );
        assert!(
            sanitized.contains("[url-redacted]"),
            "should contain URL redaction placeholder"
        );
    }

    #[test]
    fn fully_sanitize_preserves_short_clean_messages() {
        let msg = "connection reset by peer";
        let sanitized = fully_sanitize_upstream_error(msg);
        assert_eq!(sanitized, msg);
    }
}
