//! `/providers/{provider}/v1/messages` handler using the core pipeline.
//!
//! The handler parses the incoming Anthropic `MessageRequest`, decodes it into
//! a `CoreRequest` via the Anthropic client
//! adapter, and then dispatches through the shared core pipeline
//! ([`handle_core_once`] or [`handle_core_stream`]).
//!
//! Architecture boundary: route handlers do not perform scenario detection,
//! endpoint classification, fallback routing, or provider-specific streaming.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response};
use llm_proxy_core::ProviderRouteKind;
use llm_proxy_protocol::anthropic::MessageRequest;
use llm_proxy_protocol::client::anthropic;
use tracing::{info, warn};

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, route_error_response};

/// POST `/providers/{provider}/v1/messages`
///
/// Accepts an Anthropic-format [`MessageRequest`], decodes it through the
/// core pipeline, and returns an Anthropic-shaped response.
pub(crate) async fn handle_messages(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    // The wrapper only renders: it does NOT emit ResponseFailed. Each EARLY-GATE
    // error return inside the inner function emits exactly one ResponseFailed
    // itself (the sites before the core pipeline dispatch), and the core pipeline
    // emits its own single ResponseFailed for failures that occur inside it. A
    // blanket emit here would double-count every pipeline failure (the pipeline
    // emits AND returns Err via `?`), so the wrapper must stay emit-free
    // (audit route-responsefailed-gaps regression).
    match handle_messages_inner(state, meta, provider, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Anthropic, &error)
        }
    }
}

/// Inner handler that returns `Result` so errors can be mapped uniformly.
///
/// ResponseFailed emission discipline (audit route-responsefailed-gaps):
/// every EARLY-GATE error return (the sites BEFORE the core pipeline dispatch)
/// emits exactly one ResponseFailed here. Failures INSIDE the core pipeline
/// emit their own single ResponseFailed internally and return Err, which this
/// function propagates without re-emitting. The outer wrapper renders only.
async fn handle_messages_inner(
    state: AppState,
    meta: RequestMeta,
    provider: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response<Body>, RouteError> {
    let start = std::time::Instant::now();
    let event_bus = Arc::clone(&state.event_bus);
    // The request id is consumed by prepare_request below; snapshot it once so
    // every early-gate emit references the same id.
    let request_id = meta.request_id.0.clone();
    // Every early gate emits exactly one ResponseFailed with the same
    // provider/request/start triple; bind it once instead of repeating the
    // six-argument call at each gate.
    let emit_failed = |e: &RouteError| {
        core_pipeline::emit_response_failed(
            &event_bus,
            &request_id,
            Some(provider.as_str()),
            None,
            e,
            start,
        );
    };
    // Input validation gate (mirrors token_count.rs): validate the provider
    // name and confirm the provider exists BEFORE charging rate-limit/dedup
    // counters in `prepare_request`. This ensures malformed probes (bad path,
    // unknown provider) receive an immediate 400/404 without burning rate-limit
    // budget or surfacing as a misleading 429/409 (audit LOW-29).
    core_pipeline::validate_provider_name(&provider).inspect_err(&emit_failed)?;
    if state.providers().get(&provider).is_none() {
        // Unknown-provider early gate: emit exactly one ResponseFailed before
        // returning (audit route-responsefailed-gaps).
        let err = RouteError::UnknownProvider(provider.clone());
        emit_failed(&err);
        return Err(err);
    }

    // Reject non-JSON Content-Type before parsing, so a wrong media type is not
    // misreported as a JSON syntax error (audit LOW-30).
    core_pipeline::validate_json_content_type(&headers).inspect_err(&emit_failed)?;

    // Pre-flight: rate limit, dedup, request ID.
    let request_path = format!("/providers/{provider}/v1/messages");
    let ctx = core_pipeline::prepare_request(&state, &meta, &headers, &body, &request_path)
        .inspect_err(&emit_failed)?;

    // Parse the Anthropic MessageRequest via the axum Json helper so that
    // syntax-vs-data parse failures are distinguished into precise messages
    // instead of collapsing both into one opaque "invalid JSON" string
    // (audit LOW-28).
    let req: MessageRequest = match Json::<MessageRequest>::from_bytes(&body) {
        Ok(json) => json.0,
        Err(rejection) => {
            let err = json_rejection_to_route_error(rejection);
            emit_failed(&err);
            return Err(err);
        }
    };

    // Defense-in-depth: validate() checks for empty model/messages before
    // decode_request also validates the same fields. This catches issues
    // early with a clearer error message.
    req.validate()
        .map_err(|e| RouteError::InvalidRequest(e.to_string()))
        .inspect_err(&emit_failed)?;

    // Decode the Anthropic request into a core request.
    let core = anthropic::decode_request(req)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(&emit_failed)?;

    let is_streaming = core.stream;
    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming = is_streaming,
        "decoded Anthropic request into CoreRequest"
    );

    // Extract the inbound client auth token (used by passthrough-auth providers).
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::Messages,
        client_protocol: ClientProtocol::Anthropic,
    };
    if is_streaming {
        core_pipeline::handle_core_stream(state, ctx, dispatch, core, inbound_auth).await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound_auth).await
    }
}

/// Map an axum [`JsonRejection`] into a [`RouteError::InvalidRequest`] with a
/// distinct message for syntax errors (malformed JSON) versus data errors
/// (well-formed JSON that failed deserialization).
///
/// `from_bytes` cannot produce `MissingJsonContentType` (the content-type
/// header is only inspected by the `FromRequest` extractor), and `BytesRejection`
/// only arises from the request extraction path — neither is reachable here, so
/// the wildcard arm covers any future `#[non_exhaustive]` variant defensively.
fn json_rejection_to_route_error(rejection: axum::extract::rejection::JsonRejection) -> RouteError {
    match rejection {
        axum::extract::rejection::JsonRejection::JsonSyntaxError(e) => {
            RouteError::InvalidRequest(format!("invalid JSON: {e}"))
        }
        axum::extract::rejection::JsonRejection::JsonDataError(e) => {
            RouteError::InvalidRequest(format!("invalid request body: {e}"))
        }
        other => RouteError::InvalidRequest(format!("invalid JSON: {other}")),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    // -- Architecture source guard ---------------------------------------------

    #[test]
    fn source_guard_enforces_route_boundaries() {
        let source = include_str!("messages.rs");
        let prod = source
            .split_once("#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(source);

        assert!(
            !prod.contains("llm_proxy_core::router"),
            "messages.rs must not import scenario/fallback routing"
        );
        assert!(
            !prod.contains("detect_scenario"),
            "messages.rs must not use detect_scenario"
        );
        assert!(
            !prod.contains("route_for_streaming"),
            "messages.rs must not use route_for_streaming"
        );
        assert!(
            !prod.contains("classify_endpoint"),
            "messages.rs must not use classify_endpoint"
        );
        assert!(
            !prod.contains("EndpointType"),
            "messages.rs must not use EndpointType"
        );
        assert!(
            !prod.contains("OpenCodeClient"),
            "messages.rs production code must not use OpenCodeClient"
        );
        assert!(
            !prod.contains("transformer"),
            "messages.rs must not use the removed transformer module"
        );
        assert!(
            !prod.contains("StreamProxy"),
            "messages.rs must not use StreamProxy"
        );
        assert!(
            !prod.contains("spawn_proxy_task"),
            "messages.rs must not use spawn_proxy_task"
        );
        assert!(
            !prod.contains("handle_anthropic_streaming"),
            "messages.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_openai_streaming"),
            "messages.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_responses_streaming"),
            "messages.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_gemini_streaming"),
            "messages.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("ApiError"),
            "messages.rs must use RouteError"
        );
        assert!(
            !prod.contains("ScenarioConfig"),
            "messages.rs must not use ScenarioConfig"
        );
    }
}
