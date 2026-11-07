use std::sync::Arc;

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
/// Bare `/v1/messages` (plan 04 step 11): resolve the model to a provider or
/// pool, then run the same handler the provider-scoped route runs.
pub(crate) async fn handle_bare_messages(
    state: State<AppState>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    if !state.app_config().server.bare_v1.enabled {
        return route_error_response(ClientProtocol::Anthropic, &RouteError::NotFound);
    }
    let model = match core_pipeline::bare_model_of(&body) {
        Ok(model) => model,
        Err(error) => {
            warn!(error = %error, "bare /v1/messages request failed");
            return route_error_response(ClientProtocol::Anthropic, &error);
        }
    };
    match core_pipeline::resolve_bare_target(&state, &model) {
        Ok(target) => match handle_messages_inner(state.0, meta, target, headers, body).await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "request failed");
                route_error_response(ClientProtocol::Anthropic, &error)
            }
        },
        Err(error) => {
            warn!(error = %error, "bare /v1/messages target resolution failed");
            route_error_response(ClientProtocol::Anthropic, &error)
        }
    }
}

pub(crate) async fn handle_messages(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    match handle_messages_inner(state, meta, provider, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Anthropic, &error)
        }
    }
}
async fn handle_messages_inner(
    state: AppState,
    meta: RequestMeta,
    provider: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response<Body>, RouteError> {
    let start = std::time::Instant::now();
    let event_bus = Arc::clone(&state.event_bus);
    let request_id = meta.request_id.0.clone();
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
    core_pipeline::validate_provider_name(&provider).inspect_err(&emit_failed)?;
    if !core_pipeline::route_target_exists(&state, &provider) {
        let err = RouteError::UnknownProvider(provider.clone());
        emit_failed(&err);
        return Err(err);
    }
    core_pipeline::validate_json_content_type(&headers).inspect_err(&emit_failed)?;
    let ctx =
        core_pipeline::prepare_request(&state, &meta, &headers, &body).inspect_err(&emit_failed)?;
    let raw: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => value,
        Err(e) => {
            let err = RouteError::InvalidRequest(format!("invalid JSON: {e}"));
            emit_failed(&err);
            return Err(err);
        }
    };
    let req: MessageRequest = match serde::Deserialize::deserialize(raw.clone()) {
        Ok(req) => req,
        Err(e) => {
            let err = RouteError::InvalidRequest(format!("invalid request body: {e}"));
            emit_failed(&err);
            return Err(err);
        }
    };
    req.validate()
        .map_err(|e| RouteError::InvalidRequest(e.to_string()))
        .inspect_err(&emit_failed)?;
    let core = anthropic::decode_request(req)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(&emit_failed)?;
    // The raw body rides along so same-format routes can forward it verbatim
    // and the cross-format policy can name leftover fields.
    let wire = llm_proxy_adapters::ClientWireRequest::new(
        llm_proxy_adapters::ClientWireFormat::AnthropicMessages,
        raw,
    );

    let is_streaming = core.stream;
    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming = is_streaming,
        "decoded Anthropic request into CoreRequest"
    );
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::Messages,
        client_protocol: ClientProtocol::Anthropic,
    };
    let inbound = core_pipeline::InboundRequest {
        auth: inbound_auth,
        headers,
        wire,
    };
    if is_streaming {
        core_pipeline::handle_core_stream(state, ctx, dispatch, core, inbound).await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound).await
    }
}

#[cfg(test)]
mod tests {
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
        // Plan 04 step 4: pools are served at /providers/{pool}/v1/... through
        // the same engine path — the only allowed route shape stays the
        // provider-or-pool one; this file registers no routes of its own and
        // does no member picking (the engine's router owns that).
        assert!(
            !prod.contains(".route("),
            "messages.rs must not register HTTP routes; routing lives in routes/mod.rs"
        );
        assert!(
            !prod.contains("pool.members"),
            "messages.rs must not read pool members; the engine router resolves the ladder"
        );
        assert!(
            prod.contains("route_target_exists"),
            "the provider-or-pool route shape is the one allowed lookup"
        );
    }
}
