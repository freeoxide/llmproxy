//! `/providers/{provider}/v1/responses` (plan 08, step 1): the OpenAI
//! Responses client dialect — the one Codex-class clients speak.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response};
use llm_proxy_core::ProviderRouteKind;
use tracing::{info, warn};

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, route_error_response};

pub(crate) async fn handle_responses(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    match handle_responses_inner(state, meta, provider, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::OpenAiResponses, &error)
        }
    }
}

async fn handle_responses_inner(
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
    let raw: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(e) => {
            let err = RouteError::InvalidRequest(format!("invalid JSON syntax: {e}"));
            emit_failed(&err);
            return Err(err);
        }
    };
    let core = llm_proxy_protocol::client::responses::decode_request(&raw)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(&emit_failed)?;
    let wire = llm_proxy_adapters::ClientWireRequest::new(
        llm_proxy_adapters::ClientWireFormat::OpenAiResponses,
        raw,
    );

    let is_streaming = core.stream;
    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming = is_streaming,
        "decoded Responses request into CoreRequest"
    );
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::Responses,
        client_protocol: ClientProtocol::OpenAiResponses,
        ollama_generate: false,
    };
    let inbound = core_pipeline::InboundRequest {
        auth: inbound_auth,
        headers,
        wire,
    };
    if is_streaming {
        core_pipeline::handle_core_stream(
            state,
            ctx,
            dispatch,
            core,
            inbound,
            ClientProtocol::OpenAiResponses.stream_wire(),
        )
        .await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound).await
    }
}
