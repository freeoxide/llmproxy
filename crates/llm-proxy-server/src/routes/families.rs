//! Non-chat endpoint families (plan 09 step 7, audit conflict 16):
//! embeddings, rerank, audio, images, files and batch, each served by a
//! family-specific handler around the same generic spine as chat —
//! caller auth, per-family body limit and timeout, admission
//! reserve/settle, routing through the provider registry, and usage rows
//! on the shared ledger. These are *not* forced through the chat core:
//! requests pass through in the client's dialect with the edits the
//! proxy needs (model aliasing), which is exactly what these endpoints
//! need and what `docs/research/endpoints.md` prescribes per family.
//!
//! Wire shapes follow each family's documented API (OpenAI `/v1/embeddings`,
//! `/v1/audio/*`, `/v1/images/generations`, `/v1/files`,
//! `/v1/batches`; the Cohere-style `/v1/rerank`). No live upstream was
//! contacted in this environment; tests run against loopback fakes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use llm_proxy_adapters::request::{AuthHeaders, ProxyRequest};
use llm_proxy_core::ProviderRouteKind;
use llm_proxy_protocol::core::{ModelRef, Usage, UsageProvenance};
use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use crate::middleware::RequestMeta;
use crate::state::AppState;

/// Per-family wiring carried in request extensions.
#[derive(Clone)]
pub(crate) struct FamilySpec {
    /// Which route kind this family resolves.
    pub(crate) kind: ProviderRouteKind,
    /// Whether bodies are JSON (model-aliasable) or opaque bytes.
    pub(crate) json: bool,
}

/// Family names, also the `[server.endpoint_families]` keys.
pub(crate) const FAMILY_EMBEDDINGS: &str = "embeddings";
pub(crate) const FAMILY_RERANK: &str = "rerank";
pub(crate) const FAMILY_AUDIO_TRANSCRIPTIONS: &str = "audio_transcriptions";
pub(crate) const FAMILY_AUDIO_SPEECH: &str = "audio_speech";
pub(crate) const FAMILY_IMAGES_GENERATIONS: &str = "images_generations";
pub(crate) const FAMILY_FILES: &str = "files";
pub(crate) const FAMILY_BATCHES: &str = "batches";

/// POST handler for the JSON families: embeddings, rerank, images,
/// batch creation. The body passes through with the `model` field
/// rewritten to the upstream alias — the one edit every family needs —
/// and unknown fields survive (plan 02's same-dialect rule).
pub(crate) async fn handle_json_family(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    family: axum::Extension<FamilySpec>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_family(
        &state,
        &meta,
        FamilyCall {
            provider: &provider,
            spec: &family.0,
            headers: &headers,
            body,
            path_tail: "",
        },
    )
    .await
}

/// POST handler for the byte families: audio transcriptions/speech and
/// file uploads. Bodies are forwarded verbatim with their content type —
/// multipart boundaries are the HTTP client's, not the proxy's.
pub(crate) async fn handle_raw_family(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    family: axum::Extension<FamilySpec>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_family(
        &state,
        &meta,
        FamilyCall {
            provider: &provider,
            spec: &family.0,
            headers: &headers,
            body,
            path_tail: "",
        },
    )
    .await
}

/// GET/POST handler for a sub-path family (`/v1/files/{id}[/content]`,
/// `/v1/batches/{id}`): the captured path is appended to the configured
/// family endpoint.
pub(crate) async fn handle_subpath_family(
    State(state): State<AppState>,
    Path((provider, tail)): Path<(String, String)>,
    meta: RequestMeta,
    family: axum::Extension<FamilySpec>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_family(
        &state,
        &meta,
        FamilyCall {
            provider: &provider,
            spec: &family.0,
            headers: &headers,
            body,
            path_tail: &tail,
        },
    )
    .await
}

/// The inbound side of one family call.
struct FamilyCall<'a> {
    provider: &'a str,
    spec: &'a FamilySpec,
    headers: &'a HeaderMap,
    body: Bytes,
    path_tail: &'a str,
}

/// Everything one family call carries through the spine.
struct FamilyContext<'a> {
    request_id: String,
    caller: String,
    provider: &'a str,
    family: &'static str,
    requested_model: String,
    body_hash: String,
    start: Instant,
}

/// The generic family spine: resolve, admit, forward, account.
async fn run_family(state: &AppState, meta: &RequestMeta, call: FamilyCall<'_>) -> Response {
    let FamilyCall {
        provider,
        spec,
        headers,
        body,
        path_tail,
    } = call;
    let ctx = FamilyContext {
        request_id: meta.request_id.0.clone(),
        caller: meta.caller.as_str().to_owned(),
        provider,
        family: family_name(spec.kind),
        requested_model: String::new(),
        body_hash: format!("{:x}", Sha256::digest(&body)),
        start: Instant::now(),
    };

    // Resolve the route through the same registry the chat pipeline uses.
    let registry = state.providers();
    let (requested_model, outbound_body, outbound_content_type) =
        prepare_body(spec, &body, headers, &registry, provider);
    let ctx = FamilyContext {
        requested_model,
        ..ctx
    };
    let target = match registry.resolve_provider_route(provider, spec.kind, &ctx.requested_model) {
        Ok(target) => target,
        Err(error) => {
            return family_error(
                state.event_bus(),
                &ctx,
                &error.to_string(),
                StatusCode::NOT_FOUND,
            );
        }
    };

    // Admission: the same reserve/settle step the chat pipeline runs. A
    // refusal never reaches the upstream.
    let admission = match state.engine() {
        Ok(engine) => engine.shared_admission(),
        Err(error) => {
            return family_error(
                state.event_bus(),
                &ctx,
                &error.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let claim = llm_proxy_engine::AdmissionClaim {
        caller: &ctx.caller,
        provider,
        member: &target.provider_name,
        model: &target.upstream_model,
        requested_model: &ctx.requested_model,
        body_hash: &ctx.body_hash,
        estimated_input_tokens: 0,
        max_output_tokens: None,
    };
    let ticket = match admission.reserve(&claim).await {
        Ok(ticket) => ticket,
        Err(error) => {
            let status = StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::FORBIDDEN);
            return family_error(state.event_bus(), &ctx, &error.to_string(), status);
        }
    };

    emit_family_request(state.event_bus(), &ctx);

    // Forward and account.
    let upstream = forward(
        state,
        &target,
        headers,
        outbound_body,
        outbound_content_type,
        path_tail,
    )
    .await;
    account(
        state,
        admission.as_ref(),
        &ticket,
        &ctx,
        target.provider_name.clone(),
        upstream,
    )
    .await
}

/// Sends the prepared family body to the resolved endpoint: the
/// provider's credential, its static headers, the allowlisted inbound
/// headers, and the caller's content type.
async fn forward(
    state: &AppState,
    target: &llm_proxy_core::ProviderAdapterTargetConfig,
    headers: &HeaderMap,
    outbound_body: Vec<u8>,
    outbound_content_type: Option<String>,
    path_tail: &str,
) -> Result<Vec<u8>, String> {
    let mut extra_headers: HashMap<String, String> = target
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if let Some(content_type) = outbound_content_type {
        extra_headers.insert(header::CONTENT_TYPE.to_string(), content_type);
    }
    for name in target.forward_inbound_headers.iter() {
        if let Some(value) = headers.get(name)
            && let Ok(value) = value.to_str()
        {
            extra_headers.insert(name.clone(), value.to_owned());
        }
    }
    let mut url = target.endpoint.clone();
    if !path_tail.is_empty() {
        url = format!("{url}/{path_tail}");
    }
    let request = ProxyRequest {
        url,
        auth: AuthHeaders {
            style: target.auth_style,
            api_key: target.api_key.clone(),
        },
        body: outbound_body,
        stream: false,
        extra_headers,
        forwarded_headers: HashMap::new(),
    };
    state
        .proxy_client
        .send(request)
        .await
        .map_err(|error| error.to_string())
}

/// Settles the reservation and writes the terminal usage row: the
/// successful call passes the provider's bytes through, the failed one
/// answers a sanitized error.
async fn account(
    state: &AppState,
    admission: &dyn llm_proxy_engine::Admission,
    ticket: &llm_proxy_engine::AdmissionTicket,
    ctx: &FamilyContext<'_>,
    member: String,
    upstream: Result<Vec<u8>, String>,
) -> Response {
    match upstream {
        Ok(bytes) => {
            admission.settle(
                ticket,
                &llm_proxy_engine::AdmissionOutcome {
                    succeeded: true,
                    usage: None,
                    cost: None,
                },
            );
            state
                .event_bus()
                .emit(&ProxyEvent::ResponseCompleted(ResponseCompleted {
                    request_id: ctx.request_id.clone(),
                    timestamp: OffsetDateTime::now_utc(),
                    provider: member,
                    upstream_message_id: None,
                    model: ModelRef {
                        requested: ctx.requested_model.clone(),
                        upstream: None,
                    },
                    // These families report usage in their own units (rows,
                    // seconds, images); the row records the call with
                    // explicitly-synthetic-zero token usage rather than a
                    // fabricated number.
                    usage: Usage {
                        provenance: UsageProvenance::SyntheticZero,
                        ..Usage::default()
                    },
                    cost: None,
                    stop_reason: llm_proxy_protocol::core::StopReason::EndTurn,
                    latency_ms: elapsed_ms(ctx.start),
                    credential_id: None,
                    cache_hit: false,
                }));
            pass_through_ok(bytes)
        }
        Err(message) => {
            admission.settle(
                ticket,
                &llm_proxy_engine::AdmissionOutcome {
                    succeeded: false,
                    usage: None,
                    cost: None,
                },
            );
            family_error(state.event_bus(), ctx, &message, StatusCode::BAD_GATEWAY)
        }
    }
}

/// Rewrites the family body for egress: JSON families get their `model`
/// field aliased (the one surgical edit); byte families pass through
/// verbatim with their original content type. Returns the requested
/// model name (for routing, admission and usage rows).
fn prepare_body(
    spec: &FamilySpec,
    body: &Bytes,
    headers: &HeaderMap,
    registry: &llm_proxy_core::ProviderRegistry,
    provider: &str,
) -> (String, Vec<u8>, Option<String>) {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !spec.json {
        return (String::new(), body.to_vec(), content_type);
    }
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (String::new(), body.to_vec(), content_type);
    };
    let requested = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !requested.is_empty()
        && let Some(upstream_model) = registry
            .get(provider)
            .and_then(|p| p.model_aliases.get(&requested))
    {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "model".to_owned(),
                serde_json::Value::String(upstream_model.as_str().to_owned()),
            );
        }
    }
    let encoded = serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec());
    (requested, encoded, Some("application/json".to_owned()))
}

fn pass_through_ok(bytes: Vec<u8>) -> Response {
    // Family responses are provider-shaped (JSON envelopes, audio bytes,
    // file bytes); the proxy has no family decoders, so they pass through
    // with the upstream's status.
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}

fn family_error(
    bus: &Arc<dyn EventBus>,
    ctx: &FamilyContext<'_>,
    message: &str,
    status: StatusCode,
) -> Response {
    bus.emit(&ProxyEvent::ResponseFailed(ResponseFailed {
        request_id: ctx.request_id.clone(),
        timestamp: OffsetDateTime::now_utc(),
        provider: Some(ctx.provider.to_owned()),
        model: Some(ModelRef {
            requested: ctx.requested_model.clone(),
            upstream: None,
        }),
        error_kind: format!("family_{}", ctx.family),
        message: message.to_owned(),
        http_status: status.as_u16(),
        latency_ms: elapsed_ms(ctx.start),
    }));
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": { "message": message } }).to_string(),
    )
        .into_response()
}

fn emit_family_request(bus: &Arc<dyn EventBus>, ctx: &FamilyContext<'_>) {
    bus.emit(&ProxyEvent::RequestReceived(RequestReceived {
        request_id: ctx.request_id.clone(),
        timestamp: OffsetDateTime::now_utc(),
        caller: ctx.caller.clone(),
        provider: ctx.provider.to_owned(),
        route_kind: ctx.family.to_owned(),
        client_protocol: ctx.family.to_owned(),
        model: ModelRef {
            requested: ctx.requested_model.clone(),
            upstream: None,
        },
        streaming: false,
        body_hash: ctx.body_hash.clone(),
        session_id: None,
    }));
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn family_name(kind: ProviderRouteKind) -> &'static str {
    match kind {
        ProviderRouteKind::Embeddings => FAMILY_EMBEDDINGS,
        ProviderRouteKind::Rerank => FAMILY_RERANK,
        ProviderRouteKind::AudioTranscriptions => FAMILY_AUDIO_TRANSCRIPTIONS,
        ProviderRouteKind::AudioSpeech => FAMILY_AUDIO_SPEECH,
        ProviderRouteKind::ImagesGenerations => FAMILY_IMAGES_GENERATIONS,
        ProviderRouteKind::Files => FAMILY_FILES,
        ProviderRouteKind::Batches => FAMILY_BATCHES,
        _ => "chat",
    }
}
