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
enum ClientStreamEncoder {
    Anthropic(AnthropicStreamEncoder),
    OpenAi(OpenAiStreamEncoder),
}
enum ClientEncodedEvent {
    Anthropic { event_type: String, json: String },
    OpenAi { json: String },
}

impl ClientStreamEncoder {
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
    fn mark_finished(&mut self) {
        match self {
            Self::Anthropic(enc) => enc.mark_finished(),
            Self::OpenAi(enc) => enc.mark_finished(),
        }
    }
}
fn wrap_anthropic_events(
    events: Vec<llm_proxy_protocol::anthropic::MessageEvent>,
    phase: &str,
) -> Vec<ClientEncodedEvent> {
    events
        .into_iter()
        .filter_map(|me| match serde_json::to_string(&me) {
            Ok(json) => {
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
fn client_event_to_sse(encoded: ClientEncodedEvent) -> Event {
    match encoded {
        ClientEncodedEvent::Anthropic { event_type, json } => {
            Event::default().event(&event_type).data(json)
        }
        ClientEncodedEvent::OpenAi { json } => Event::default().data(json),
    }
}
fn openai_done_event() -> Event {
    Event::default().data("[DONE]")
}
#[derive(Debug)]
pub(crate) struct RequestContext {
    pub(crate) request_id: String,
    pub(crate) start: Instant,
    pub(crate) caller: String,
    pub(crate) body_hash: String,
}
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
const MAX_PROVIDER_NAME_LEN: usize = 128;
static PROVIDER_NAME_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^[a-z0-9_-]+$").expect("valid provider name regex")
});
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
pub(super) fn validate_json_content_type(headers: &HeaderMap) -> Result<(), RouteError> {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return Ok(());
    };
    let Ok(ct) = value.to_str() else {
        return Ok(());
    };
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
fn validate_provider_name_ref(provider_name: &str) -> Result<&str, RouteError> {
    validate_provider_name(provider_name)?;
    Ok(provider_name)
}
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
    if let Some(provider) = providers.get(provider_name) {
        if provider
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog.enforce)
        {
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
        .clone();
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
fn route_kind_str(kind: ProviderRouteKind) -> &'static str {
    match kind {
        ProviderRouteKind::ChatCompletions => "chat_completions",
        ProviderRouteKind::Messages => "messages",
    }
}
fn client_protocol_str(protocol: ClientProtocol) -> &'static str {
    match protocol {
        ClientProtocol::OpenAiChat => "openai_chat",
        ClientProtocol::Anthropic => "anthropic",
    }
}
pub(crate) fn body_hash_of_bytes(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    format!("{:x}", hasher.finalize())
}
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
fn capture_lifecycle(ctx: &mut StreamContext, event: &CoreEvent) {
    match event {
        CoreEvent::MessageStart { id: Some(id), .. } => {
            ctx.upstream_message_id = Some(id.clone());
        }
        CoreEvent::UsageDelta { usage } => {
            ctx.pending_usage = Some(usage.clone());
        }
        CoreEvent::MessageStop { stop_reason, .. } => {
            ctx.stop_reason = Some(stop_reason.clone());
        }
        _ => {}
    }
}
pub(crate) fn extract_inbound_auth(headers: &HeaderMap) -> Option<SecretString> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
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
fn compute_cost(usage: &Usage, pricing: &ModelPricing) -> Cost {
    fn priced(count: i32, price: Decimal) -> Decimal {
        Decimal::from(count)
            .checked_mul(price)
            .unwrap_or(Decimal::MAX)
            .round_dp(6)
    }
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
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoreDispatch<'a> {
    pub(crate) provider_name: &'a str,
    pub(crate) route_kind: ProviderRouteKind,
    pub(crate) client_protocol: ClientProtocol,
}
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

    let response_bytes = state
        .upstream_client_for(&target.provider_name)
        .send(proxy_req)
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
    let latency = ctx.start.elapsed();
    let pricing = state
        .providers()
        .pricing_for(&target.provider_name, &target.upstream_model)
        .cloned();
    let cost = if core_resp.usage.provenance == UsageProvenance::ProviderReported {
        pricing.map(|p| compute_cost(&core_resp.usage, &p))
    } else {
        None
    };
    core_resp.cost = cost.clone();
    let resp_usage = core_resp.usage.clone();
    let resp_model = core_resp.model.clone();
    let resp_upstream_id = core_resp.id.clone();
    let resp_stop_reason = core_resp.stop_reason.clone();
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
    state
        .metrics
        .record_success(&target.provider_name, &target.upstream_model, latency);
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
    response.headers_mut().insert(
        "x-request-id",
        ctx.request_id
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );

    Ok(response)
}
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
enum FirstByteResult {
    FirstEvent(Event),
    PreStreamError(RouteError),
}
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
    let byte_stream = state
        .upstream_client_for(&target.provider_name)
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
    let request_id = ctx.request_id;
    let upstream_model = target.upstream_model.clone();
    let pricing = state
        .providers()
        .pricing_for(&target.provider_name, &target.upstream_model)
        .cloned();
    let event_bus = Arc::clone(&state.event_bus);
    let requested_model = core.model.clone();
    let stream_start = ctx.start;
    let (first_byte_tx, first_byte_rx) = tokio::sync::oneshot::channel::<FirstByteResult>();
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
    let first_event = first_byte_rx.await.map_err(|error| {
        let err = RouteError::Internal(format!(
            "stream task exited before first event ({error}: possible panic, cancellation, or empty stream)"
        ));
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
            let stream =
                futures::stream::once(async move { Ok::<_, std::convert::Infallible>(event) })
                    .chain(output_stream.map(Ok::<_, std::convert::Infallible>));

            let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(HEARTBEAT_INTERVAL));

            let response = sse.into_response();
            let (mut parts, body) = response.into_parts();
            parts.headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            parts.headers.insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            );
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
#[derive(Debug)]
struct StreamMetrics {
    metrics: Arc<Metrics>,
    provider_name: String,
    upstream_model: String,
    start: Instant,
}
struct StreamContext {
    provider_decoder: ProviderStreamDecoderKind,
    sse_framer: SseFramer,
    client_protocol: ClientProtocol,
    core: CoreRequest,
    request_id: String,
    stream_metrics: StreamMetrics,
    first_byte_tx: Option<tokio::sync::oneshot::Sender<FirstByteResult>>,
    first_byte_sent: bool,
    pricing: Option<ModelPricing>,
    passthrough_auth: bool,
    event_bus: Arc<dyn EventBus>,
    pending_usage: Option<Usage>,
    stop_reason: Option<StopReason>,
    upstream_message_id: Option<String>,
}

impl StreamContext {
    async fn emit_event(&mut self, event: Event, tx: &tokio::sync::mpsc::Sender<Event>) -> bool {
        if !self.first_byte_sent {
            if let Some(fb_tx) = self.first_byte_tx.take() {
                if fb_tx.send(FirstByteResult::FirstEvent(event)).is_ok() {
                    self.first_byte_sent = true;
                    return true;
                }
                self.stream_metrics.metrics.record_client_cancel();
                return false;
            }
            warn!(
                request_id = %self.request_id,
                "emit_event: first-byte channel already consumed but no first \
                 byte sent; stopping stream task to avoid silently dropping an event"
            );
            return false;
        } else if tx.send(event).await.is_err() {
            self.stream_metrics.metrics.record_client_cancel();
            self.emit_client_cancelled();
            return false;
        }
        true
    }
    fn send_pre_stream_error(&mut self, error: RouteError) -> bool {
        if let Some(fb_tx) = self.first_byte_tx.take() {
            fb_tx.send(FirstByteResult::PreStreamError(error)).is_ok()
        } else {
            false
        }
    }
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
    if matches!(ctx.client_protocol, ClientProtocol::OpenAiChat)
        && !ctx.emit_event(openai_done_event(), tx).await
    {
        return false;
    }

    true
}
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
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(256);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
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
        let mut stream_errored = false;
        let mut pending: Vec<CoreEvent> = Vec::new();
        'buffer: loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
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
        for core_event in pending.drain(..) {
            capture_lifecycle(&mut ctx, &core_event);
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
                let Some(json_str) = openai_chat::stream_error_sse_payload(&stream_error) else {
                    continue;
                };
                if !ctx.emit_event(Event::default().data(json_str), &tx).await {
                    return;
                }
                if !ctx.emit_event(openai_done_event(), &tx).await {
                    return;
                }
                encoder.mark_finished();
                stream_succeeded = false;
                stream_errored = true;
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
        if !stream_errored {
        'outer: loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    ctx.stream_metrics.metrics.record_client_cancel();
                    if !ctx.first_byte_sent {
                        ctx.send_pre_stream_error(
                            RouteError::Internal("client disconnected before first byte".to_owned()),
                        );
                    } else {
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
                                let sanitized = fully_sanitize_upstream_error(&e.to_string());
                                emit_stream_error(
                                    &mut encoder,
                                    &tx,
                                    &ctx.client_protocol,
                                    &sanitized,
                                ).await;
                                ctx.stream_metrics.metrics.record_failure();
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
                            break 'outer;
                        }
                    }
                }
            }
        }
        }
        if !stream_errored && ctx.first_byte_sent {
            if !finalize_sse_framer(&mut ctx, &mut encoder, &tx).await {
                return;
            }
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
        if stream_succeeded {
            ctx.stream_metrics.metrics.record_success(
                &ctx.stream_metrics.provider_name,
                &ctx.stream_metrics.upstream_model,
                ctx.stream_metrics.start.elapsed(),
            );
            let usage = ctx
                .pending_usage
                .clone()
                .unwrap_or_else(Usage::synthetic_zero);
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
    let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let cancel_guard = cancel.drop_guard();
    rx_stream
        .map(move |item| {
            let _guard = &cancel_guard;
            item
        })
        .boxed()
}
fn extract_upstream_message_id(events: &[CoreEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        CoreEvent::MessageStart { id: Some(id), .. } => Some(id.clone()),
        _ => None,
    })
}
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
    if matches!(client_protocol, ClientProtocol::OpenAiChat)
        && tx.send(openai_done_event()).await.is_err()
    {
        return;
    }
    client_encoder.mark_finished();
}
pub(crate) fn map_provider_error(
    e: &llm_proxy_provider::error::ProviderError,
    passthrough_auth: bool,
) -> RouteError {
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
            if e.is_timeout() {
                let sanitized = fully_sanitize_upstream_error(&e.to_string());
                return RouteError::UpstreamTimeout(sanitized);
            }
            let sanitized = fully_sanitize_upstream_error(&e.to_string());
            RouteError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: sanitized,
                auth_owner,
            }
        }
    }
}
const MAX_SANITIZE_LEN: usize = 512;
const SANITIZE_SUFFIX: &str = "...[truncated]";
static URL_REDACT_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"https?://\S+").expect("valid URL redaction regex")
});
pub(super) fn sanitize_upstream_error_body(msg: &str) -> String {
    let truncated = truncate_with_suffix(msg, MAX_SANITIZE_LEN, SANITIZE_SUFFIX);
    URL_REDACT_REGEX
        .replace_all(&truncated, "[url-redacted]")
        .into_owned()
}
pub(super) fn fully_sanitize_upstream_error(msg: &str) -> String {
    let key_redacted = match llm_proxy_provider::error::ProviderError::api(0, msg.to_owned()) {
        llm_proxy_provider::error::ProviderError::Api { body, .. } => body,
        _ => return sanitize_upstream_error_body(msg),
    };
    sanitize_upstream_error_body(&key_redacted)
}
pub(crate) fn protocol_error_to_route(e: llm_proxy_protocol::client::ProtocolError) -> RouteError {
    match e {
        llm_proxy_protocol::client::ProtocolError::InvalidRequest(msg) => {
            RouteError::InvalidRequest(msg)
        }
        llm_proxy_protocol::client::ProtocolError::Decode(msg) => RouteError::InvalidRequest(msg),
        other => RouteError::Internal(other.to_string()),
    }
}

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
        assert_eq!(cost.input.normalize().to_string(), "0.0015");
        assert_eq!(cost.output.normalize().to_string(), "0.0015");
        assert_eq!(cost.cache_creation.normalize().to_string(), "0.000375");
        assert_eq!(cost.cache_read.normalize().to_string(), "0");
        assert_eq!(cost.reasoning.normalize().to_string(), "0.00015");
        assert_eq!(cost.total.normalize().to_string(), "0.003525");
    }

    #[test]
    fn compute_cost_rounds_to_microcent() {
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
        assert_eq!(cost.input.normalize().to_string(), "0.000006");
        assert_eq!(cost.total.normalize().to_string(), "0.000006");
    }

    #[test]
    fn compute_cost_saturates_on_overflow_and_never_panics() {
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
        let cost = compute_cost(&usage, &pricing);
        assert_eq!(cost.input, Decimal::MAX);
        assert_eq!(cost.total, Decimal::MAX);
    }
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
        assert_eq!(recorded.error_kind, "Upstream");
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
        let json = serde_json::to_string(&ProxyEvent::ResponseFailed(recorded)).unwrap();
        assert!(
            !json.contains("sk-ant-api03-") && !json.contains("ABCDEFGHIJKLMNOP"),
            "secret value leaked into event JSON: {json}"
        );
    }
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
        assert_eq!(
            extracted_token("Bearer   abc123   ").as_deref(),
            Some("abc123")
        );
        assert_eq!(extracted_token("Basic dXNlcjpwYXNz"), None);
        assert_eq!(extracted_token("Bearer "), None);
    }

    #[test]
    fn body_hash_is_stable_and_distinguishes_requests() {
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
                admin_bind: None,
                admin_on_main_listener: false,
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
                admin_bind: None,
                admin_on_main_listener: false,
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
                admin_bind: None,
                admin_on_main_listener: false,
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

    #[test]
    fn encode_core_event_ping_produces_event() {
        use llm_proxy_protocol::core::CoreEvent;
        let mut encoder = ClientStreamEncoder::Anthropic(AnthropicStreamEncoder::new(
            "msg_test".to_owned(),
            "test-model".to_owned(),
        ));
        let events = encode_core_event(&mut encoder, CoreEvent::Ping);
        let _ = events;
    }

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

    #[test]
    fn fully_sanitize_redacts_api_key_patterns() {
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
