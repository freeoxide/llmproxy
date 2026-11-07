//! `/providers/{provider}/v1beta/models/{model}:generateContent` and
//! `:streamGenerateContent` (plan 08, step 2): the Gemini native client
//! dialect. The model lives in the URL and streaming in the method; the
//! stream framing is SSE with `alt=sse`, otherwise a JSON array of chunks
//! (both documented Gemini shapes).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Response};
use llm_proxy_core::ProviderRouteKind;
use tracing::{info, warn};

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, StreamWire, route_error_response};

/// `alt=sse` selects SSE framing for `streamGenerateContent`; any other
/// value (or none) keeps the default JSON-array-of-chunks framing.
fn wants_sse(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    query.split('&').any(|pair| pair == "alt=sse")
}

pub(crate) async fn handle_generate_content(
    State(state): State<AppState>,
    Path((provider, model_method)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let (model, streaming) = match model_method.rsplit_once(':') {
        Some((model, "generateContent")) => (model.to_owned(), false),
        Some((model, "streamGenerateContent")) => (model.to_owned(), true),
        _ => {
            // Anything else (`:countTokens`, `:embedContent`, a bare model
            // name) is not a route this proxy serves.
            let error = RouteError::NotFound;
            let start = std::time::Instant::now();
            core_pipeline::emit_response_failed(
                &state.event_bus,
                &meta.request_id.0,
                Some(provider.as_str()),
                None,
                &error,
                start,
            );
            return route_error_response(ClientProtocol::Gemini, &error);
        }
    };
    match handle_generate_inner(
        state,
        meta,
        provider,
        model,
        streaming,
        query.as_deref(),
        headers,
        body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Gemini, &error)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the handler mirrors the route's extractors (provider, model, method, query, \
              headers, body) plus the shared request context; grouping them would invent \
              a struct only this call site uses"
)]
async fn handle_generate_inner(
    state: AppState,
    meta: RequestMeta,
    provider: String,
    model: String,
    streaming: bool,
    query: Option<&str>,
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
    let raw: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| RouteError::InvalidRequest(format!("invalid JSON syntax: {e}")))
        .inspect_err(&emit_failed)?;
    let core = llm_proxy_protocol::client::gemini::decode_request(&raw, &model, streaming)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(&emit_failed)?;
    let wire = llm_proxy_adapters::ClientWireRequest::new(
        llm_proxy_adapters::ClientWireFormat::GeminiGenerateContent,
        raw,
    );

    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming,
        "decoded Gemini generateContent request into CoreRequest"
    );
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::GenerateContent,
        client_protocol: ClientProtocol::Gemini,
        ollama_generate: false,
    };
    let inbound = core_pipeline::InboundRequest {
        auth: inbound_auth,
        headers,
        wire,
    };
    if streaming {
        let wire_framing = if wants_sse(query) {
            StreamWire::SseData
        } else {
            StreamWire::JsonArray
        };
        core_pipeline::handle_core_stream(state, ctx, dispatch, core, inbound, wire_framing).await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound).await
    }
}
