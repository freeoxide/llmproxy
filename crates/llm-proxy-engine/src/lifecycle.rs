//! The request lifecycle (plan 03's pipeline slots), transport- and
//! HTTP-free: admission, routing, credentials, guards, cache, the attempt
//! loop against one upstream, settlement. This module was extracted from the
//! axum layer so the server, an SDK embedder and the bindings all run the
//! same pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use llm_proxy_adapters::client_wire::ClientWireRequest;
use llm_proxy_adapters::request::ProxyRequest;
use llm_proxy_adapters::sse::SseFramer;
use llm_proxy_adapters::{ProtocolName, ProviderStreamDecoder};
use llm_proxy_core::{ModelPricing, ProviderRouteKind};
use llm_proxy_protocol::core::{
    CoreEvent, CoreRequest, Cost, ModelRef, StopReason, Usage, UsageProvenance,
};
use llm_proxy_storage::{ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed};
use rust_decimal::Decimal;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::error::{EngineError, map_provider_error, sanitize_upstream_error_body};
use crate::headers::select_forwarded_headers;
use crate::traits::{
    AdmissionOutcome, AdmissionTicket, CacheEntry, CacheKey, GuardDecision, RouteQuery, UsageSink,
};
use crate::types::{ClientDialect, EngineRequest, EngineResponse, EngineStream};
use crate::{EngineInner, ProviderAdapterTarget};

/// Client-facing message when the upstream ends a stream without a terminal
/// event; deliberately generic (no upstream body) since it is sent in-band.
const TRUNCATED_STREAM_MESSAGE: &str = "upstream stream ended before the response completed";

/// Client-facing message for a stream that failed to decode after the first
/// byte was committed; generic because it is sent in-band.
const PROVIDER_DECODE_CLIENT_MESSAGE: &str = "provider response decode error";

/// Maps an encode failure to an engine error: a cross-format policy rejection
/// (`ProviderError::InvalidRequest`) is the client's fault (400 naming the
/// fields); anything else is internal.
fn map_encode_error(e: llm_proxy_adapters::ProviderError) -> EngineError {
    match e {
        llm_proxy_adapters::ProviderError::InvalidRequest(message) => {
            EngineError::InvalidRequest(message)
        }
        other => EngineError::Internal(format!("encode error: {other}")),
    }
}

pub(crate) fn route_kind_str(kind: ProviderRouteKind) -> &'static str {
    match kind {
        ProviderRouteKind::ChatCompletions => "chat_completions",
        ProviderRouteKind::Messages => "messages",
    }
}

pub(crate) fn client_dialect_str(dialect: ClientDialect) -> &'static str {
    match dialect {
        ClientDialect::OpenAiChat => "openai_chat",
        ClientDialect::Anthropic => "anthropic",
    }
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

pub(crate) fn emit_request_received(inner: &EngineInner, req: &EngineRequest, streaming: bool) {
    inner
        .usage
        .record(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: req.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: req.caller.clone(),
            provider: req.provider_name.clone(),
            route_kind: route_kind_str(req.route_kind).to_owned(),
            client_protocol: client_dialect_str(req.client_dialect).to_owned(),
            model: req.core.model.clone(),
            streaming,
            body_hash: req.body_hash.clone(),
        }));
}

/// Emits a [`ProxyEvent::ResponseFailed`] row for `error`.
pub(crate) fn emit_response_failed(
    inner: &EngineInner,
    request_id: &str,
    provider: Option<&str>,
    model: Option<&ModelRef>,
    error: &EngineError,
    start: Instant,
) {
    let provider = provider.or(match error {
        EngineError::UnknownProvider(name) => Some(name.as_str()),
        _ => None,
    });
    if let EngineError::Upstream {
        status,
        body,
        auth_owner,
    } = error
    {
        tracing::warn!(
            upstream_status = status,
            auth_owner = ?auth_owner,
            upstream_body = %body,
            "upstream error mapped for the client; real body logged here only"
        );
    }
    inner
        .usage
        .record(&ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: request_id.to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: provider.map(str::to_owned),
            model: model.cloned(),
            error_kind: error.kind_name().to_owned(),
            message: error.client_message(),
            http_status: error.status_code(),
            latency_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }));
}

/// One resolved attempt: the target plus the codec that serves it.
struct Attempt {
    target: ProviderAdapterTarget,
    adapter: Arc<dyn llm_proxy_adapters::ProviderAdapter>,
}

async fn resolve_attempt(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    passthrough_auth: bool,
) -> Result<Attempt, EngineError> {
    let resolved = inner
        .router
        .resolve(&RouteQuery {
            provider: &req.provider_name,
            route_kind: req.route_kind,
            requested_model: &req.core.model.requested,
        })
        .await?;
    let configured = resolved.target;

    let adapter = inner.adapters.get(&configured.protocol).ok_or_else(|| {
        EngineError::Internal(format!(
            "no adapter registered for protocol: {}",
            configured.protocol
        ))
    })?;
    let api_key = inner
        .credentials
        .credential(
            &configured.provider_name,
            &configured.api_key,
            passthrough_auth,
            req.inbound.auth.as_ref(),
        )
        .await?;

    let target = ProviderAdapterTarget {
        provider_name: configured.provider_name,
        adapter_name: configured.adapter_name,
        protocol: ProtocolName::new(&configured.protocol),
        endpoint: configured.endpoint,
        auth_style: configured.auth_style,
        api_key,
        requested_model: configured.requested_model,
        upstream_model: configured.upstream_model,
        headers: Arc::clone(&configured.headers),
        lossy: configured.lossy,
        forward_inbound_headers: Arc::clone(&configured.forward_inbound_headers),
    };
    Ok(Attempt { target, adapter })
}

fn ledger_model(core_model: &ModelRef, upstream_model: &str) -> ModelRef {
    let mut model = core_model.clone();
    if model.upstream.is_none() && upstream_model != model.requested {
        model.upstream = Some(upstream_model.to_owned());
    }
    model
}

fn settle(inner: &EngineInner, ticket: &AdmissionTicket, succeeded: bool, usage: Option<&Usage>) {
    inner
        .admission
        .settle(ticket, &AdmissionOutcome { succeeded, usage });
}

/// Non-streaming execution.
pub(crate) async fn execute(
    inner: Arc<EngineInner>,
    req: EngineRequest,
) -> Result<EngineResponse, EngineError> {
    let start = req.start;
    crate::validate_provider_name(&req.provider_name)?;
    inner.metrics.record_request(false);
    let passthrough_auth = inner
        .providers
        .get(&req.provider_name)
        .is_some_and(|p| p.passthrough_auth);

    emit_request_received(&inner, &req, false);

    let outcome = run_once(&inner, &req, passthrough_auth).await;
    match outcome {
        Ok(response) => Ok(response),
        Err(error) => {
            inner.metrics.record_failure();
            emit_response_failed(
                &inner,
                &req.request_id,
                Some(&req.provider_name),
                Some(&req.core.model),
                &error,
                start,
            );
            Err(error)
        }
    }
}

async fn run_once(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    passthrough_auth: bool,
) -> Result<EngineResponse, EngineError> {
    // Slot 3: resolve the route.
    let attempt = resolve_attempt(inner, req, passthrough_auth).await?;

    let cache_key = CacheKey {
        caller: &req.caller,
        provider: &attempt.target.provider_name,
        route_kind: req.route_kind,
        model: &attempt.target.upstream_model,
        body_hash: &req.body_hash,
        streaming: false,
    };
    // Slot 5: cache lookup (and, later, single-flight join). Only complete
    // non-streaming responses are served from the cache in this plan.
    if let Some(entry) = inner
        .cache
        .lookup(&cache_key)
        .await
        .map_err(|error| EngineError::Internal(format!("cache lookup: {error}")))?
        && entry.events.is_empty()
    {
        tracing::debug!(
            request_id = %req.request_id,
            provider = %attempt.target.provider_name,
            "cache hit"
        );
        let mut cached = entry.response;
        let usage = cached.usage.clone();
        let stop_reason = cached.stop_reason.clone();
        let upstream_message_id = cached.id.clone();
        let cost = cached.cost.clone();
        inner
            .usage
            .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
                request_id: req.request_id.clone(),
                timestamp: time::OffsetDateTime::now_utc(),
                provider: attempt.target.provider_name.clone(),
                upstream_message_id,
                model: ledger_model(&req.core.model, &attempt.target.upstream_model),
                usage,
                cost,
                stop_reason,
                latency_ms: 0,
            }));
        cached.model = ledger_model(&req.core.model, &attempt.target.upstream_model);
        return Ok(EngineResponse {
            core: cached,
            raw_body: None,
        });
    }

    // Slot 6: reserve budget (default admits everything).
    let claim = crate::traits::AdmissionClaim {
        caller: &req.caller,
        provider: &attempt.target.provider_name,
        model: &attempt.target.upstream_model,
        body_hash: &req.body_hash,
    };
    let ticket = inner.admission.reserve(&claim).await?;

    // Slots 7-13 run against the reservation. The Admission contract
    // settles exactly once per reserve, so the reserved section settles on
    // success while every failure below — a guard error or rejection,
    // encode, transport, decode — funnels through one settle-failed here.
    match run_reserved(inner, req, &attempt, &ticket, passthrough_auth, &cache_key).await {
        Ok(response) => Ok(response),
        Err(error) => {
            settle(inner, &ticket, false, None);
            Err(error)
        }
    }
}

/// Slots 7-13 of the non-streaming lifecycle, against a reserved ticket:
/// the success path settles it; every error propagates to [`run_once`],
/// which settles it as failed.
async fn run_reserved(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    attempt: &Attempt,
    ticket: &AdmissionTicket,
    passthrough_auth: bool,
    cache_key: &CacheKey<'_>,
) -> Result<EngineResponse, EngineError> {
    // Slot 7: pre-call guards.
    for guard in &inner.guards {
        match guard.inspect_request(&req.core).await? {
            GuardDecision::Allow => {}
            GuardDecision::Reject(message) => {
                return Err(EngineError::InvalidRequest(message));
            }
        }
    }

    // Slot 8/9: encode and send.
    let mut wire: ClientWireRequest = req.inbound.wire.clone();
    wire.forwarded_headers = select_forwarded_headers(
        &req.inbound.headers,
        &attempt.target.forward_inbound_headers,
    );
    let proxy_req: ProxyRequest = attempt
        .adapter
        .encode_request(&req.core, &attempt.target, &wire)
        .map_err(map_encode_error)?;

    let response_bytes = inner
        .transport
        .client_for(&req.provider_name)
        .send(proxy_req)
        .await
        .map_err(|e| map_provider_error(&e, passthrough_auth))?;

    let mut core_resp = attempt
        .adapter
        .decode_response(&response_bytes, &attempt.target)
        .map_err(|e| EngineError::ProviderDecode(format!("decode response: {e}")))?;

    // Slot 11: post-call guards.
    for guard in &inner.guards {
        match guard.inspect_response(&core_resp).await? {
            GuardDecision::Allow => {}
            GuardDecision::Reject(message) => {
                return Err(EngineError::InvalidRequest(message));
            }
        }
    }

    let latency = req.start.elapsed();
    let pricing = inner
        .providers
        .pricing_for(
            &attempt.target.provider_name,
            &attempt.target.upstream_model,
        )
        .cloned();
    let cost = if core_resp.usage.provenance == UsageProvenance::ProviderReported {
        pricing.as_ref().map(|p| compute_cost(&core_resp.usage, p))
    } else {
        None
    };
    core_resp.cost = cost.clone();

    // Slot 13: settle, store, record.
    settle(inner, ticket, true, Some(&core_resp.usage));
    let entry = CacheEntry {
        response: core_resp.clone(),
        events: Vec::new(),
    };
    inner.cache.store(cache_key, &entry).await;

    let resp_usage = core_resp.usage.clone();
    let resp_model = core_resp.model.clone();
    let resp_upstream_id = core_resp.id.clone();
    let resp_stop_reason = core_resp.stop_reason.clone();
    inner.metrics.record_success(
        &attempt.target.provider_name,
        &attempt.target.upstream_model,
        latency,
    );
    inner
        .usage
        .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: req.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: attempt.target.provider_name.clone(),
            upstream_message_id: resp_upstream_id,
            model: ledger_model(&resp_model, &attempt.target.upstream_model),
            usage: resp_usage,
            cost,
            stop_reason: resp_stop_reason,
            latency_ms: latency.as_millis().try_into().unwrap_or(u64::MAX),
        }));

    // Same-format passthrough: the upstream response body reaches the client
    // verbatim; the decode above only read usage and the stop reason for the
    // ledger, it did not rewrite the body (conflict 10). Cross-format clients
    // get the body re-encoded into their dialect by the server.
    let raw_body = if wire.same_format_as(&attempt.target.protocol) {
        if attempt.target.upstream_model != req.core.model.requested {
            tracing::debug!(
                request_id = %req.request_id,
                "same-format passthrough: the response advertises the upstream model name"
            );
        }
        Some(response_bytes)
    } else {
        None
    };

    Ok(EngineResponse {
        core: core_resp,
        raw_body,
    })
}

/// What the first-byte window reports: the buffered prefix that crosses the
/// pre-commit boundary, or the error that aborts before any byte is
/// committed.
enum FirstByteResult {
    Head(Vec<CoreEvent>),
    PreStreamError(EngineError),
}

type ByteStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<Bytes, llm_proxy_adapters::ProviderError>> + Send>,
>;

/// Streaming execution. Returns only once the first core events are decoded
/// (the pre-commit window: the caller can still return an HTTP error status)
/// and hands back the buffered prefix plus the continuation stream.
/// Settlement happens inside the stream as it completes or is cancelled.
pub(crate) async fn execute_stream(
    inner: Arc<EngineInner>,
    req: EngineRequest,
) -> Result<EngineStream, EngineError> {
    let start = req.start;
    crate::validate_provider_name(&req.provider_name)?;
    inner.metrics.record_request(true);
    let passthrough_auth = inner
        .providers
        .get(&req.provider_name)
        .is_some_and(|p| p.passthrough_auth);

    emit_request_received(&inner, &req, true);

    let prepared = prepare_stream(&inner, &req, passthrough_auth).await;
    let (byte_stream, attempt, ticket) = match prepared {
        Ok(parts) => parts,
        Err(error) => {
            inner.metrics.record_failure();
            emit_response_failed(
                &inner,
                &req.request_id,
                Some(&req.provider_name),
                Some(&req.core.model),
                &error,
                start,
            );
            return Err(error);
        }
    };

    let pricing = inner
        .providers
        .pricing_for(
            &attempt.target.provider_name,
            &attempt.target.upstream_model,
        )
        .cloned();
    let provider_name = attempt.target.provider_name.clone();
    let upstream_model = attempt.target.upstream_model.clone();
    let request_id = req.request_id.clone();
    let requested_model = req.core.model.clone();

    let (first_byte_tx, first_byte_rx) = tokio::sync::oneshot::channel::<FirstByteResult>();
    let failure_recorded = Arc::new(AtomicBool::new(false));
    let decoder = attempt.adapter.new_stream_decoder(&attempt.target);
    let core_stream = build_core_event_stream(
        Arc::clone(&inner),
        StreamContext {
            decoder,
            sse_framer: SseFramer::new(),
            core: req.core.clone(),
            request_id: request_id.clone(),
            provider_name: provider_name.clone(),
            upstream_model: upstream_model.clone(),
            start,
            first_byte_tx: Some(first_byte_tx),
            first_byte_sent: false,
            pricing,
            passthrough_auth,
            pending_usage: None,
            stop_reason: None,
            upstream_message_id: None,
            ticket,
        },
        byte_stream,
        Arc::clone(&failure_recorded),
    );

    let first_head = first_byte_rx.await.map_err(|error| {
        let err = EngineError::Internal(format!(
            "stream task exited before first event ({error}: possible panic, cancellation, or empty stream)"
        ));
        if !failure_recorded.swap(true, Ordering::SeqCst) {
            inner.metrics.record_failure();
            emit_response_failed(
                &inner,
                &request_id,
                Some(provider_name.as_str()),
                Some(&requested_model),
                &err,
                start,
            );
        }
        err
    })?;

    match first_head {
        FirstByteResult::PreStreamError(error) => {
            emit_response_failed(
                &inner,
                &request_id,
                Some(provider_name.as_str()),
                Some(&requested_model),
                &error,
                start,
            );
            Err(error)
        }
        FirstByteResult::Head(head) => Ok(EngineStream {
            head,
            tail: core_stream,
        }),
    }
}

async fn prepare_stream(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    passthrough_auth: bool,
) -> Result<(ByteStream, Attempt, AdmissionTicket), EngineError> {
    let attempt = resolve_attempt(inner, req, passthrough_auth).await?;

    // Streaming responses are not served from the cache in this plan; cache
    // semantics for streams land with the cache plan.
    let claim = crate::traits::AdmissionClaim {
        caller: &req.caller,
        provider: &attempt.target.provider_name,
        model: &attempt.target.upstream_model,
        body_hash: &req.body_hash,
    };
    let ticket = inner.admission.reserve(&claim).await?;

    // Same settle funnel as `run_once`: once the byte stream is returned the
    // stream task owns settlement; every failure before that — a guard error
    // or rejection, encode, transport — settles as failed here.
    match stream_reserved(inner, req, attempt, passthrough_auth).await {
        Ok((byte_stream, attempt)) => Ok((byte_stream, attempt, ticket)),
        Err(error) => {
            settle(inner, &ticket, false, None);
            Err(error)
        }
    }
}

/// The reserved section of a streaming attempt: pre-call guards, encode,
/// egress. Settlement of the handed-back stream belongs to the stream task;
/// every error here propagates to [`prepare_stream`], which settles the
/// ticket as failed.
async fn stream_reserved(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    attempt: Attempt,
    passthrough_auth: bool,
) -> Result<(ByteStream, Attempt), EngineError> {
    for guard in &inner.guards {
        match guard.inspect_request(&req.core).await? {
            GuardDecision::Allow => {}
            GuardDecision::Reject(message) => {
                return Err(EngineError::InvalidRequest(message));
            }
        }
    }

    let mut wire: ClientWireRequest = req.inbound.wire.clone();
    wire.forwarded_headers = select_forwarded_headers(
        &req.inbound.headers,
        &attempt.target.forward_inbound_headers,
    );
    let proxy_req: ProxyRequest = attempt
        .adapter
        .encode_request(&req.core, &attempt.target, &wire)
        .map_err(map_encode_error)?;

    let byte_stream = inner
        .transport
        .client_for(&req.provider_name)
        .send_stream(proxy_req)
        .await
        .map_err(|e| map_provider_error(&e, passthrough_auth))?;
    Ok((byte_stream, attempt))
}

struct StreamContext {
    decoder: Box<dyn ProviderStreamDecoder + Send>,
    sse_framer: SseFramer,
    core: CoreRequest,
    request_id: String,
    provider_name: String,
    upstream_model: String,
    start: Instant,
    first_byte_tx: Option<tokio::sync::oneshot::Sender<FirstByteResult>>,
    first_byte_sent: bool,
    pricing: Option<ModelPricing>,
    passthrough_auth: bool,
    pending_usage: Option<Usage>,
    stop_reason: Option<StopReason>,
    upstream_message_id: Option<String>,
    ticket: AdmissionTicket,
}

impl StreamContext {
    fn send_pre_stream_error(&mut self, error: EngineError) -> bool {
        if let Some(fb_tx) = self.first_byte_tx.take() {
            fb_tx.send(FirstByteResult::PreStreamError(error)).is_ok()
        } else {
            false
        }
    }

    /// The model ref recorded on ledger rows: the requested model plus the
    /// upstream model actually serving it, so alias rows stay attributable.
    fn ledger_model(&self) -> ModelRef {
        ledger_model(&self.core.model, &self.upstream_model)
    }
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

/// Yields the in-band `CoreEvent::Error` for a post-commit failure; the
/// client codec renders it (an OpenAI client gets an error chunk, an
/// Anthropic client an `error` event) and closes cleanly.
async fn emit_in_band_error(tx: &tokio::sync::mpsc::Sender<CoreEvent>, message: &str) -> bool {
    let error = llm_proxy_protocol::core::CoreStreamError::new(
        llm_proxy_protocol::core::CoreStreamErrorKind::Upstream,
        message.to_owned(),
    );
    tx.send(CoreEvent::Error { error }).await.is_ok()
}

fn record_stream_success(inner: &EngineInner, ctx: &StreamContext) {
    inner
        .metrics
        .record_success(&ctx.provider_name, &ctx.upstream_model, ctx.start.elapsed());
    let usage = ctx
        .pending_usage
        .clone()
        .unwrap_or_else(Usage::synthetic_zero);
    let cost = if usage.provenance == UsageProvenance::ProviderReported {
        ctx.pricing.as_ref().map(|p| compute_cost(&usage, p))
    } else {
        None
    };
    inner
        .usage
        .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: ctx.provider_name.clone(),
            upstream_message_id: ctx.upstream_message_id.clone(),
            model: ctx.ledger_model(),
            usage,
            cost,
            stop_reason: ctx.stop_reason.clone().unwrap_or(StopReason::EndTurn),
            latency_ms: ctx
                .start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }));
}

fn record_client_cancelled(inner: &EngineInner, ctx: &StreamContext) {
    inner
        .usage
        .record(&ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: Some(ctx.provider_name.clone()),
            model: Some(ctx.ledger_model()),
            error_kind: "ClientCancelled".to_owned(),
            message: "client disconnected before the stream completed".to_owned(),
            http_status: 200,
            latency_ms: ctx
                .start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }));
}

/// The sanitized message of a `CoreEvent::Error`, for ledger rows.
fn core_event_message(event: &CoreEvent) -> &str {
    match event {
        CoreEvent::Error { error } => error.message(),
        _ => "",
    }
}

/// Captures lifecycle state for and forwards the final events; `false` when
/// the client went away.
///
/// Holds `&mut StreamContext` across the awaits (the send backs up on a slow
/// client); `&mut` of a `Send` struct is `Send`, unlike `&`.
async fn emit_final_events(
    inner: &Arc<EngineInner>,
    ctx: &mut StreamContext,
    tx: &tokio::sync::mpsc::Sender<CoreEvent>,
    events: Vec<CoreEvent>,
) -> bool {
    for core_event in events {
        capture_lifecycle(ctx, &core_event);
        if tx.send(core_event).await.is_err() {
            record_client_cancelled(inner, ctx);
            settle(inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
            return false;
        }
    }
    true
}

/// Drives the upstream byte stream to core events. Mirrors the pre-split
/// stream state machine: buffer until the first events cross the first-byte
/// window, decode and forward events, mark in-band failures, finalize with
/// the decoder's terminal, settle the ledger, and supervise the task.
#[expect(
    clippy::too_many_lines,
    reason = "one streaming state machine: the select-loop's cancellation, \
              decode, first-byte, and disconnect transitions read as a unit; \
              splitting the arms into helpers would scatter the state they \
              mutate"
)]
fn build_core_event_stream(
    inner: Arc<EngineInner>,
    ctx: StreamContext,
    byte_stream: ByteStream,
    failure_recorded: Arc<AtomicBool>,
) -> BoxStream<'static, CoreEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<CoreEvent>(256);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let supervise_request_id = ctx.request_id.clone();
    let supervise_provider = ctx.provider_name.clone();
    let supervise_model = ctx.core.model.clone();
    let supervise_start = ctx.start;
    let stream_span = tracing::info_span!("stream_task", request_id = %supervise_request_id);

    let mut stream = byte_stream;
    let mut ctx = ctx;
    let task_inner = Arc::clone(&inner);
    let stream_task = tokio::spawn(
        async move {
            let inner = task_inner;
            let mut stream_succeeded = false;
            let mut stream_errored = false;
            let mut stream_failed = false;
            let mut pending: Vec<CoreEvent> = Vec::new();
            'buffer: loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => {
                        inner.metrics.record_client_cancel();
                        if !ctx.send_pre_stream_error(
                            EngineError::Internal("client disconnected before first byte".to_owned()),
                        ) {
                            // The caller is gone, so nobody else will settle the
                            // RequestReceived row; write it here.
                            record_client_cancelled(&inner, &ctx);
                            settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                        }
                        return;
                    }
                    chunk = stream.next() => match chunk {
                        Some(Ok(bytes)) => {
                            let frames = match ctx.sse_framer.push_chunk(&bytes) {
                                Ok(frames) => frames,
                                Err(e) => {
                                    tracing::warn!(
                                        request_id = %ctx.request_id,
                                        error = %e,
                                        "SSE framing error in stream"
                                    );
                                    inner.metrics.record_failure();
                                    ctx.send_pre_stream_error(EngineError::ProviderDecode(
                                        format!("stream framing error: {e}"),
                                    ));
                                    settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                                    return;
                                }
                            };
                            for frame in &frames {
                                match ctx.decoder.decode_frame(frame) {
                                    Ok(events) => pending.extend(events),
                                    Err(e) => {
                                        tracing::warn!(
                                            request_id = %ctx.request_id,
                                            error = %e,
                                            "provider decode error in stream"
                                        );
                                        inner.metrics.record_failure();
                                        ctx.send_pre_stream_error(EngineError::ProviderDecode(
                                            format!("provider decode error: {e}"),
                                        ));
                                        settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                                        return;
                                    }
                                }
                            }
                            if !pending.is_empty() {
                                break 'buffer;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                request_id = %ctx.request_id,
                                error = %e,
                                "upstream stream error"
                            );
                            inner.metrics.record_failure();
                            ctx.send_pre_stream_error(map_provider_error(&e, ctx.passthrough_auth));
                            settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                            return;
                        }
                        None => break 'buffer,
                    }
                }
            }

            if pending.is_empty() {
                // The upstream ended with no events at all: nothing crossed
                // the pre-commit window, so the caller still gets an HTTP
                // error instead of an empty 200 stream.
                ctx.send_pre_stream_error(EngineError::ProviderDecode(
                    "upstream returned an empty stream with no events".to_owned(),
                ));
                settle(&inner, &ctx.ticket, false, None);
                return;
            }

            // Slot 11: streaming guards vet the buffered prefix before any
            // byte is committed; a rejection still maps to an HTTP error.
            for core_event in &pending {
                for guard in &inner.guards {
                    match guard.inspect_event(core_event).await {
                        Ok(GuardDecision::Allow) => {}
                        Ok(GuardDecision::Reject(message)) => {
                            inner.metrics.record_failure();
                            ctx.send_pre_stream_error(EngineError::InvalidRequest(message));
                            settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                            return;
                        }
                        Err(error) => {
                            inner.metrics.record_failure();
                            ctx.send_pre_stream_error(error);
                            settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                            return;
                        }
                    }
                }
            }
            for core_event in pending.iter() {
                capture_lifecycle(&mut ctx, core_event);
            }
            // A leading in-band error is terminal: the pre-commit window
            // crossed with an error, so the upstream is dropped (not consumed
            // further), the failure is recorded, and the error event is the
            // client's first output.
            if pending.iter().any(|event| matches!(event, CoreEvent::Error { .. })) {
                let message = pending
                    .iter()
                    .map(core_event_message)
                    .find(|message| !message.is_empty())
                    .unwrap_or_default()
                    .to_owned();
                inner.metrics.record_failure();
                if let Some(fb_tx) = ctx.first_byte_tx.take() {
                    ctx.first_byte_sent = true;
                    if fb_tx.send(FirstByteResult::Head(pending)).is_err() {
                        inner.metrics.record_client_cancel();
                        record_client_cancelled(&inner, &ctx);
                        settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                        return;
                    }
                }
                emit_response_failed(
                    &inner,
                    &ctx.request_id,
                    Some(ctx.provider_name.as_str()),
                    Some(&ctx.ledger_model()),
                    &EngineError::ProviderDecode(sanitize_upstream_error_body(&message)),
                    ctx.start,
                );
                settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                return;
            }
            // The buffered prefix crosses the first-byte window as one batch;
            // the caller sizes its client encoder from it (message id, etc).
            if let Some(fb_tx) = ctx.first_byte_tx.take() {
                ctx.first_byte_sent = true;
                if fb_tx.send(FirstByteResult::Head(pending)).is_err() {
                    inner.metrics.record_client_cancel();
                    record_client_cancelled(&inner, &ctx);
                    settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                    return;
                }
            }
            if !stream_errored {
            'outer: loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => {
                        inner.metrics.record_client_cancel();
                        if !ctx.first_byte_sent {
                            if !ctx.send_pre_stream_error(EngineError::Internal(
                                "client disconnected before first byte".to_owned(),
                            )) {
                                record_client_cancelled(&inner, &ctx);
                            }
                        } else {
                            record_client_cancelled(&inner, &ctx);
                        }
                        settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                        return;
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                let frames = match ctx.sse_framer.push_chunk(&bytes) {
                                    Ok(frames) => frames,
                                    Err(e) => {
                                        tracing::warn!(
                                            request_id = %ctx.request_id,
                                            error = %e,
                                            "SSE framing error in stream"
                                        );
                                        let error = EngineError::ProviderDecode(format!(
                                            "stream framing error: {e}"
                                        ));
                                        inner.metrics.record_failure();
                                        if !ctx.first_byte_sent {
                                            ctx.send_pre_stream_error(error);
                                        } else {
                                            if !emit_in_band_error(&tx, PROVIDER_DECODE_CLIENT_MESSAGE).await {
                                                record_client_cancelled(&inner, &ctx);
                                            }
                                            emit_response_failed(
                                                &inner,
                                                &ctx.request_id,
                                                Some(ctx.provider_name.as_str()),
                                                Some(&ctx.ledger_model()),
                                                &error,
                                                ctx.start,
                                            );
                                        }
                                        stream_errored = true;
                                        break 'outer;
                                    }
                                };

                                for frame in &frames {
                                    let core_events = match ctx.decoder.decode_frame(frame) {
                                        Ok(events) => events,
                                        Err(e) => {
                                            tracing::warn!(
                                                request_id = %ctx.request_id,
                                                error = %e,
                                                "provider decode error in stream"
                                            );
                                            let error = EngineError::ProviderDecode(
                                                format!("provider decode error: {e}"),
                                            );
                                            inner.metrics.record_failure();
                                            if !ctx.first_byte_sent {
                                                ctx.send_pre_stream_error(error);
                                            } else {
                                                if !emit_in_band_error(&tx, PROVIDER_DECODE_CLIENT_MESSAGE).await {
                                                record_client_cancelled(&inner, &ctx);
                                            }
                                                emit_response_failed(
                                                    &inner,
                                                    &ctx.request_id,
                                                    Some(ctx.provider_name.as_str()),
                                                    Some(&ctx.ledger_model()),
                                                    &error,
                                                    ctx.start,
                                                );
                                            }
                                            stream_errored = true;
                                            break 'outer;
                                        }
                                    };

                                    for core_event in core_events {
                                        capture_lifecycle(&mut ctx, &core_event);
                                        if matches!(core_event, CoreEvent::Error { .. }) {
                                            // In-band upstream failure after visible
                                            // output: mark it for the client and the
                                            // ledger, then keep the framing valid.
                                            stream_failed = true;
                                            inner.metrics.record_failure();
                                            emit_response_failed(
                                                &inner,
                                                &ctx.request_id,
                                                Some(ctx.provider_name.as_str()),
                                                Some(&ctx.ledger_model()),
                                                &EngineError::ProviderDecode(
                                                    sanitize_upstream_error_body(
                                                        core_event_message(&core_event),
                                                    ),
                                                ),
                                                ctx.start,
                                            );
                                            if tx.send(core_event).await.is_err() {
                                                record_client_cancelled(&inner, &ctx);
                                                settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                                                return;
                                            }
                                            continue;
                                        }
                                        if tx.send(core_event).await.is_err() {
                                            record_client_cancelled(&inner, &ctx);
                                            settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                                            return;
                                        }
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(
                                    request_id = %ctx.request_id,
                                    error = %e,
                                    "upstream stream error"
                                );
                                let error = map_provider_error(&e, ctx.passthrough_auth);
                                inner.metrics.record_failure();
                                if !ctx.first_byte_sent {
                                    ctx.send_pre_stream_error(error);
                                } else {
                                    let sanitized = crate::error::fully_sanitize_upstream_error(
                                        &e.to_string(),
                                    );
                                    if !emit_in_band_error(&tx, &sanitized).await {
                                        record_client_cancelled(&inner, &ctx);
                                    }
                                    emit_response_failed(
                                        &inner,
                                        &ctx.request_id,
                                        Some(ctx.provider_name.as_str()),
                                        Some(&ctx.ledger_model()),
                                        &error,
                                        ctx.start,
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
                // Flush any trailing partial SSE frame.
                let trailing_frames = match ctx.sse_framer.finish() {
                    Ok(frames) => frames,
                    Err(e) => {
                        tracing::warn!(
                            request_id = %ctx.request_id,
                            error = %e,
                            "SSE framer finish error"
                        );
                        Vec::new()
                    }
                };
                for frame in &trailing_frames {
                    let events = match ctx.decoder.decode_frame(frame) {
                        Ok(events) => events,
                        Err(e) => {
                            tracing::warn!(
                                request_id = %ctx.request_id,
                                error = %e,
                                "provider decode error on trailing frame"
                            );
                            break;
                        }
                    };
                    if !emit_final_events(&inner, &mut ctx, &tx, events).await {
                        return;
                    }
                }
                if !ctx.decoder.saw_terminal() {
                    // The upstream ended without a terminal event. Keep valid
                    // client framing (synthetic terminal below) but mark the
                    // failure in-band and in the ledger.
                    stream_failed = true;
                    inner.metrics.record_failure();
                    emit_response_failed(
                        &inner,
                        &ctx.request_id,
                        Some(ctx.provider_name.as_str()),
                        Some(&ctx.ledger_model()),
                        &EngineError::ProviderDecode(TRUNCATED_STREAM_MESSAGE.to_owned()),
                        ctx.start,
                    );
                    let error = llm_proxy_protocol::core::CoreStreamError::new(
                        llm_proxy_protocol::core::CoreStreamErrorKind::Upstream,
                        TRUNCATED_STREAM_MESSAGE.to_owned(),
                    );
                    if tx.send(CoreEvent::Error { error }).await.is_err() {
                        record_client_cancelled(&inner, &ctx);
                        settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
                        return;
                    }
                }
                match ctx.decoder.finish() {
                    Ok(final_events) => {
                        if !emit_final_events(&inner, &mut ctx, &tx, final_events).await {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            request_id = %ctx.request_id,
                            error = %e,
                            "provider decoder finish error"
                        );
                    }
                }
                if !stream_failed {
                    stream_succeeded = true;
                }
            }
            if stream_succeeded {
                record_stream_success(&inner, &ctx);
                settle(&inner, &ctx.ticket, true, ctx.pending_usage.as_ref());
            } else {
                settle(&inner, &ctx.ticket, false, ctx.pending_usage.as_ref());
            }
        }
        .instrument(stream_span),
    );

    tokio::spawn(async move {
        match stream_task.await {
            Ok(()) => {}
            Err(join_error) => {
                let panicked = join_error.is_panic();
                let error = EngineError::Internal(format!(
                    "stream task {}",
                    if panicked {
                        "panicked"
                    } else {
                        "was cancelled"
                    }
                ));
                tracing::warn!(
                    request_id = %supervise_request_id,
                    error = %join_error,
                    panicked,
                    "detached stream task did not complete cleanly"
                );
                if !failure_recorded.swap(true, Ordering::SeqCst) {
                    inner.metrics.record_failure();
                    emit_response_failed(
                        &inner,
                        &supervise_request_id,
                        Some(supervise_provider.as_str()),
                        Some(&supervise_model),
                        &error,
                        supervise_start,
                    );
                }
            }
        }
    });
    let rx_stream = futures::stream::unfold(rx, |mut rx| async move {
        let received = rx.recv().await;
        received.map(|event| (event, rx))
    });
    let cancel_guard = cancel.drop_guard();
    rx_stream
        .map(move |item| {
            let _guard = &cancel_guard;
            item
        })
        .boxed()
}
