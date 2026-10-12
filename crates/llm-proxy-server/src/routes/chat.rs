//! `/providers/{provider}/v1/chat/completions` handler using the core pipeline.
//!
//! The handler parses the incoming OpenAI `ChatCompletionRequest`, decodes it
//! into a `CoreRequest` via the OpenAI Chat
//! client adapter, and then dispatches through the shared core pipeline
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
use llm_proxy_protocol::client::openai_chat;
use llm_proxy_protocol::openai::ChatCompletionRequest;
use tracing::{info, warn};

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, route_error_response};

/// POST `/providers/{provider}/v1/chat/completions`
///
/// Accepts an OpenAI-format [`ChatCompletionRequest`], decodes it through the
/// core pipeline, and returns an OpenAI-shaped response.
pub(crate) async fn handle_chat_completions(
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
    match handle_chat_completions_inner(state, meta, provider, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::OpenAiChat, &error)
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
async fn handle_chat_completions_inner(
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
    // Input validation gates, ordered cheapest-first to avoid charging
    // rate-limit/dedup budget for malformed requests. Mirrors the ordering
    // established in `token_count.rs`: provider name -> existence ->
    // content-type -> rate-limit/dedup -> JSON parse.
    core_pipeline::validate_provider_name(&provider).inspect_err(&emit_failed)?;
    if state.providers().get(&provider).is_none() {
        // Unknown-provider early gate: emit exactly one ResponseFailed before
        // returning. provider is known from the path (audit route-responsefailed-gaps).
        let err = RouteError::UnknownProvider(provider.clone());
        emit_failed(&err);
        return Err(err);
    }
    core_pipeline::validate_json_content_type(&headers).inspect_err(&emit_failed)?;

    let request_path = format!("/providers/{provider}/v1/chat/completions");
    let ctx = core_pipeline::prepare_request(&state, &meta, &headers, &body, &request_path)
        .inspect_err(&emit_failed)?;

    // Parse the OpenAI ChatCompletionRequest via axum's `Json` helper so the
    // `JsonRejection` taxonomy (syntax vs data error) is preserved and mapped
    // to a precise 400 body, rather than collapsing both into one opaque
    // "invalid JSON" string.
    let req: ChatCompletionRequest = match Json::<ChatCompletionRequest>::from_bytes(&body) {
        Ok(Json(value)) => value,
        Err(rejection) => {
            let err = json_rejection_to_route_error(rejection);
            emit_failed(&err);
            return Err(err);
        }
    };

    // Decode the OpenAI Chat request into a core request.
    // Note: ChatCompletionRequest does not have a separate validate() method
    // (unlike the Anthropic handler). All validation is performed inside
    // decode_request: it checks for non-empty model and non-empty messages.
    let core = openai_chat::decode_request(req)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(|e| emit_failed(e))?;

    let is_streaming = core.stream;
    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming = is_streaming,
        "decoded OpenAI Chat request into CoreRequest"
    );

    // Extract the inbound client auth token (used by passthrough-auth providers).
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::ChatCompletions,
        client_protocol: ClientProtocol::OpenAiChat,
    };
    if is_streaming {
        core_pipeline::handle_core_stream(state, ctx, dispatch, core, inbound_auth).await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound_auth).await
    }
}

/// Map an axum `JsonRejection` to a `RouteError`, preserving the distinction
/// between a syntactically invalid JSON body and a body that is valid JSON but
/// does not fit the target type.
fn json_rejection_to_route_error(rejection: axum::extract::rejection::JsonRejection) -> RouteError {
    use axum::extract::rejection::JsonRejection;
    match rejection {
        JsonRejection::JsonSyntaxError(e) => {
            RouteError::InvalidRequest(format!("invalid JSON syntax: {e}"))
        }
        JsonRejection::JsonDataError(e) => {
            RouteError::InvalidRequest(format!("JSON body did not match expected type: {e}"))
        }
        JsonRejection::MissingJsonContentType(_) => {
            RouteError::InvalidRequest("missing application/json Content-Type".to_owned())
        }
        // BytesRejection and any future non_exhaustive variant: surface a
        // generic but informative invalid-request body.
        other => RouteError::InvalidRequest(format!("could not read request body: {other}")),
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
        let source = include_str!("chat.rs");
        let prod = source
            .split_once("#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(source);

        assert!(
            !prod.contains("llm_proxy_core::router"),
            "chat.rs must not import scenario/fallback routing"
        );
        assert!(
            !prod.contains("detect_scenario"),
            "chat.rs must not use detect_scenario"
        );
        assert!(
            !prod.contains("route_for_streaming"),
            "chat.rs must not use route_for_streaming"
        );
        assert!(
            !prod.contains("classify_endpoint"),
            "chat.rs must not use classify_endpoint"
        );
        assert!(
            !prod.contains("EndpointType"),
            "chat.rs must not use EndpointType"
        );
        assert!(
            !prod.contains("OpenCodeClient"),
            "chat.rs production code must not use OpenCodeClient"
        );
        assert!(
            !prod.contains("transformer"),
            "chat.rs must not use the removed transformer module"
        );
        assert!(
            !prod.contains("StreamProxy"),
            "chat.rs must not use StreamProxy"
        );
        assert!(
            !prod.contains("spawn_proxy_task"),
            "chat.rs must not use spawn_proxy_task"
        );
        assert!(
            !prod.contains("handle_anthropic_streaming"),
            "chat.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_openai_streaming"),
            "chat.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_responses_streaming"),
            "chat.rs must not use provider-specific stream handlers"
        );
        assert!(
            !prod.contains("handle_gemini_streaming"),
            "chat.rs must not use provider-specific stream handlers"
        );
        assert!(!prod.contains("ApiError"), "chat.rs must use RouteError");
        assert!(
            !prod.contains("ScenarioConfig"),
            "chat.rs must not use ScenarioConfig"
        );
        assert!(
            !prod.contains("axum_serde"),
            "chat.rs must not use the removed echo handler"
        );
        assert!(
            !prod.contains("Sonic"),
            "chat.rs must not use the removed echo handler"
        );
    }
}
