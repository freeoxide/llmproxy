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
//! `/v1/audio/*`, `/v1/images/generations`, `/v1/files`, `/v1/batches`;
//! the Cohere-style `/v1/rerank`), verified against real implementations
//! (`tests/live_families.rs`: llama.cpp, speaches, stable-diffusion.cpp
//! and Llama Stack, plus the official OpenAI and Cohere SDKs against the
//! daemon):
//!
//! - The client's method and query reach the upstream: files and batches
//!   are a lifecycle of `GET` lists and downloads, `DELETE`s and `POST`s,
//!   not one upload. A resource path (`/files/{id}/content`) goes between
//!   the configured endpoint and its own query, which survives.
//! - A resource path is plain identifier segments. Anything a URL parser
//!   would resolve (`..`, `%2e%2e`) could leave the family's endpoint on
//!   the operator's credential, so it is refused before any upstream call.
//! - The answer keeps the upstream's status, `Content-Type` and
//!   `Content-Disposition`: speech is audio, a transcript may be plain
//!   text, a download is the file.
//! - The credential and transport are the provider's, as on the chat
//!   routes: `passthrough_auth` sends the client's own key,
//!   `[provider.egress]` proxies the family's egress too.
//! - Failures follow the one client-status policy
//!   ([`llm_proxy_engine::map_upstream_status`]): the upstream's error body
//!   never reaches the client. One stated addition: an upstream `404` for
//!   a request that names a resource (`/files/{id}`, `/batches/{id}`) is
//!   that resource's absence — the client's fixable `404`, not an upstream
//!   fault. A collection endpoint that `404`s is still a misconfigured
//!   upstream (`502`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use llm_proxy_adapters::ProviderError;
use llm_proxy_adapters::request::{AuthHeaders, ProxyRequest};
use llm_proxy_core::ProviderRouteKind;
use llm_proxy_engine::EngineError;
use llm_proxy_protocol::core::{ModelRef, Usage, UsageProvenance};
use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived, ResponseCompleted};
use llm_proxy_transport::ProxyClient;
use llm_proxy_transport::client::UpstreamResponse;
use secrecy::SecretString;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use super::core_pipeline::{emit_response_failed, extract_inbound_auth};
use super::error_response::{ClientProtocol, RouteError, route_error_response};
use crate::middleware::RequestMeta;
use crate::state::AppState;

/// Per-family wiring carried in request extensions.
#[derive(Clone)]
pub(crate) struct FamilySpec {
    /// Which route kind this family resolves.
    pub(crate) kind: ProviderRouteKind,
    /// Whether request bodies are JSON (model-aliasable) or opaque bytes
    /// (multipart uploads). Speech is JSON: only its answer is audio.
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

/// A family's collection route (`/v1/embeddings`, `/v1/files`, ...).
#[expect(
    clippy::too_many_arguments,
    reason = "axum handlers take one argument per extractor; this route reads the method, URI and headers"
)]
pub(crate) async fn handle_family(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    family: axum::Extension<FamilySpec>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_family(
        &state,
        &meta,
        FamilyCall {
            provider: &provider,
            spec: &family.0,
            method,
            query: uri.query(),
            headers: &headers,
            body,
            resource: "",
        },
    )
    .await
}

/// A family's resource route (`/v1/files/{id}[/content]`,
/// `/v1/batches/{id}[/cancel]`): the captured path is appended to the
/// configured family endpoint.
#[expect(
    clippy::too_many_arguments,
    reason = "axum handlers take one argument per extractor; this route reads the method, URI and headers"
)]
pub(crate) async fn handle_family_resource(
    State(state): State<AppState>,
    Path((provider, resource)): Path<(String, String)>,
    meta: RequestMeta,
    family: axum::Extension<FamilySpec>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_family(
        &state,
        &meta,
        FamilyCall {
            provider: &provider,
            spec: &family.0,
            method,
            query: uri.query(),
            headers: &headers,
            body,
            resource: &resource,
        },
    )
    .await
}

/// The inbound side of one family call.
struct FamilyCall<'a> {
    provider: &'a str,
    spec: &'a FamilySpec,
    method: Method,
    query: Option<&'a str>,
    headers: &'a HeaderMap,
    body: Bytes,
    /// The resource path after the family endpoint; empty for the
    /// collection itself.
    resource: &'a str,
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

/// The prepared egress side of one family call.
struct Outbound<'a> {
    method: Method,
    url: String,
    body: Vec<u8>,
    content_type: Option<String>,
    headers: &'a HeaderMap,
}

/// The generic family spine: resolve, admit, forward, account.
async fn run_family(state: &AppState, meta: &RequestMeta, call: FamilyCall<'_>) -> Response {
    let FamilyCall {
        provider,
        spec,
        method,
        query,
        headers,
        body,
        resource,
    } = call;
    let mut ctx = FamilyContext {
        request_id: meta.request_id.0.clone(),
        caller: meta.caller.as_str().to_owned(),
        provider,
        family: family_name(spec.kind),
        requested_model: String::new(),
        body_hash: format!("{:x}", Sha256::digest(&body)),
        start: Instant::now(),
    };

    if !resource.is_empty() && !is_plain_resource_path(resource) {
        return family_error(
            state.event_bus(),
            &ctx,
            &RouteError::InvalidRequest(
                "a resource path is identifier segments ([A-Za-z0-9._~-], no dot segments)"
                    .to_owned(),
            ),
        );
    }

    // Resolve the route through the same registry the chat pipeline uses.
    let registry = state.providers();
    let (requested_model, outbound_body, outbound_content_type) =
        prepare_body(spec, &body, headers, &registry, provider);
    ctx.requested_model = requested_model;
    let target = match registry.resolve_provider_route(provider, spec.kind, &ctx.requested_model) {
        Ok(target) => target,
        Err(error) => {
            let refusal = RouteError::from(EngineError::from_route_error(error));
            return family_error(state.event_bus(), &ctx, &refusal);
        }
    };

    // The credential, as the chat routes pick it: the provider's key, or
    // on a `passthrough_auth` provider the client's own, which it must
    // bring. (A multi-key pool's extra keys are not rotated here: family
    // calls use the base key.)
    let passthrough = registry
        .get(&target.provider_name)
        .is_some_and(|config| config.passthrough_auth);
    let api_key = if passthrough {
        match extract_inbound_auth(headers) {
            Some(key) => key,
            None => return family_error(state.event_bus(), &ctx, &RouteError::Unauthorized),
        }
    } else {
        target.api_key.clone()
    };

    // Admission: the same reserve/settle step the chat pipeline runs. A
    // refusal never reaches the upstream.
    let engine = match state.engine() {
        Ok(engine) => engine,
        Err(error) => {
            let refusal = RouteError::Internal(error.to_string());
            return family_error(state.event_bus(), &ctx, &refusal);
        }
    };
    let admission = engine.shared_admission();
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
        Err(error) => return family_error(state.event_bus(), &ctx, &RouteError::from(error)),
    };

    emit_family_request(state.event_bus(), &ctx);

    // Forward on the provider's own transport (its `[provider.egress]`
    // client when it has one) and account.
    let outbound = Outbound {
        method,
        url: family_url(&target.endpoint, resource, query),
        body: outbound_body,
        content_type: outbound_content_type,
        headers,
    };
    let client = engine.transport().client_for(&target.provider_name);
    let upstream = forward(client, &target, api_key, outbound).await;
    account(
        state,
        admission.as_ref(),
        &ticket,
        &ctx,
        target.provider_name.clone(),
        upstream.map_err(|error| upstream_refusal(&error, !resource.is_empty(), passthrough)),
    )
    .await
}

/// Whether `path` is one or more identifier segments: unreserved URL
/// characters only and no `.`/`..` segment, so no URL parser can resolve
/// it outside the family endpoint it is appended to.
fn is_plain_resource_path(path: &str) -> bool {
    path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
    })
}

/// The upstream URL for one call: the configured endpoint with the
/// resource path appended before the endpoint's own query, then the
/// client's query after it.
fn family_url(endpoint: &str, resource: &str, client_query: Option<&str>) -> String {
    let (base, endpoint_query) = match endpoint.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (endpoint, None),
    };
    let mut url = base.to_owned();
    if !resource.is_empty() {
        if !url.ends_with('/') {
            url.push('/');
        }
        url.push_str(resource);
    }
    let queries: Vec<&str> = [endpoint_query, client_query]
        .into_iter()
        .flatten()
        .filter(|query| !query.is_empty())
        .collect();
    if !queries.is_empty() {
        url.push('?');
        url.push_str(&queries.join("&"));
    }
    url
}

/// Sends the prepared family call to the resolved endpoint: the client's
/// method, the picked credential, the provider's static headers, the
/// allowlisted inbound headers, and the caller's content type.
async fn forward(
    client: &ProxyClient,
    target: &llm_proxy_core::ProviderAdapterTargetConfig,
    api_key: SecretString,
    outbound: Outbound<'_>,
) -> Result<UpstreamResponse, ProviderError> {
    let mut extra_headers: HashMap<String, String> = target
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if let Some(content_type) = outbound.content_type {
        extra_headers.insert(header::CONTENT_TYPE.to_string(), content_type);
    }
    for name in target.forward_inbound_headers.iter() {
        if let Some(value) = outbound.headers.get(name)
            && let Ok(value) = value.to_str()
        {
            extra_headers.insert(name.clone(), value.to_owned());
        }
    }
    let request = ProxyRequest {
        url: outbound.url,
        auth: AuthHeaders {
            style: target.auth_style,
            api_key,
        },
        body: outbound.body,
        stream: false,
        extra_headers,
        forwarded_headers: HashMap::new(),
    };
    client.send_raw(outbound.method, request).await
}

/// The client-facing refusal for an upstream failure: the one status
/// policy, plus the resource rule — a request that names a resource and
/// is answered `404` asked for something the provider does not have.
fn upstream_refusal(
    error: &ProviderError,
    names_a_resource: bool,
    passthrough: bool,
) -> RouteError {
    if names_a_resource && matches!(error, ProviderError::Api { status: 404, .. }) {
        return RouteError::NotFound;
    }
    RouteError::from(llm_proxy_engine::map_provider_error(error, passthrough))
}

/// Settles the reservation and writes the terminal usage row: the
/// successful call passes the provider's answer through, the failed one
/// answers the policy's refusal.
async fn account(
    state: &AppState,
    admission: &dyn llm_proxy_engine::Admission,
    ticket: &llm_proxy_engine::AdmissionTicket,
    ctx: &FamilyContext<'_>,
    member: String,
    upstream: Result<UpstreamResponse, RouteError>,
) -> Response {
    match upstream {
        Ok(answer) => {
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
            pass_through(answer)
        }
        Err(refusal) => {
            admission.settle(
                ticket,
                &llm_proxy_engine::AdmissionOutcome {
                    succeeded: false,
                    usage: None,
                    cost: None,
                },
            );
            family_error(state.event_bus(), ctx, &refusal)
        }
    }
}

/// Rewrites the family body for egress: JSON families get their `model`
/// field aliased (the one surgical edit); byte families pass through
/// verbatim with their original content type, and a bodiless call
/// (a `GET`, a `DELETE`, a cancel) stays bodiless. Returns the requested
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
    if !spec.json || body.is_empty() {
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

/// The provider's answer as the client receives it: its status, the
/// headers that describe the body, and the body. Family answers are
/// provider-shaped (JSON envelopes, audio, files); the proxy has no
/// family decoders and relabels nothing.
fn pass_through(answer: UpstreamResponse) -> Response {
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::OK);
    let mut response = (status, answer.body).into_response();
    for (name, value) in [
        (header::CONTENT_TYPE, answer.content_type),
        (header::CONTENT_DISPOSITION, answer.content_disposition),
    ] {
        if let Some(value) = value.and_then(|value| HeaderValue::from_str(&value).ok()) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// One refusal, rendered once: the failure row, then the OpenAI-dialect
/// error body with the policy's `x-should-retry` and `Retry-After`.
fn family_error(bus: &Arc<dyn EventBus>, ctx: &FamilyContext<'_>, error: &RouteError) -> Response {
    let model = ModelRef {
        requested: ctx.requested_model.clone(),
        upstream: None,
    };
    emit_response_failed(
        bus,
        &ctx.request_id,
        Some(ctx.provider),
        Some(&model),
        error,
        ctx.start,
    );
    route_error_response(ClientProtocol::OpenAiChat, error)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_goes_between_the_endpoint_and_its_query() {
        assert_eq!(
            family_url("https://h/v1/files", "file-1/content", None),
            "https://h/v1/files/file-1/content"
        );
        assert_eq!(
            family_url(
                "https://h/v1/files?api-version=1",
                "file-1",
                Some("limit=2")
            ),
            "https://h/v1/files/file-1?api-version=1&limit=2"
        );
        assert_eq!(
            family_url("https://h/v1/batches/", "b_1/cancel", Some("")),
            "https://h/v1/batches/b_1/cancel",
            "no doubled slash, no empty query"
        );
        assert_eq!(
            family_url("https://h/v1/files", "", Some("purpose=batch")),
            "https://h/v1/files?purpose=batch"
        );
    }

    #[test]
    fn only_identifier_segments_form_a_resource_path() {
        for accepted in ["file-abc", "file-abc/content", "batch_1/cancel", "v1.2~x"] {
            assert!(is_plain_resource_path(accepted), "{accepted}");
        }
        for refused in [
            "..", "a/../b", "./a", "a//b", "a/", "%2e%2e", "a/%2E%2e", "a?b=c", "a#b", "a b",
            "a\\..\\b", "",
        ] {
            assert!(!is_plain_resource_path(refused), "{refused:?}");
        }
    }
}
