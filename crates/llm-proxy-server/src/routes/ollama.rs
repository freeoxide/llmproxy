//! Ollama-native client routes (plan 08, step 3): `/api/chat`, `/api/generate`
//! and `/api/tags` under `/providers/{provider}`. Both generation routes
//! resolve through the provider's `chat_completions` adapter (they are chat
//! endpoints reached in the Ollama dialect) and reuse the same admission
//! middleware chain.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::response::IntoResponse;
use llm_proxy_core::ProviderRouteKind;
use tracing::{info, warn};

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, route_error_response};

pub(crate) async fn handle_ollama_chat(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    match handle_generation_inner(state, meta, provider, headers, body, false).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Ollama, &error)
        }
    }
}

pub(crate) async fn handle_ollama_generate(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    match handle_generation_inner(state, meta, provider, headers, body, true).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Ollama, &error)
        }
    }
}

async fn handle_generation_inner(
    state: AppState,
    meta: RequestMeta,
    provider: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
    generate: bool,
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
    let core = if generate {
        llm_proxy_protocol::client::ollama::decode_generate_request(&raw)
    } else {
        llm_proxy_protocol::client::ollama::decode_chat_request(&raw)
    }
    .map_err(core_pipeline::protocol_error_to_route)
    .inspect_err(&emit_failed)?;
    let wire = llm_proxy_adapters::ClientWireRequest::new(
        if generate {
            llm_proxy_adapters::ClientWireFormat::OllamaGenerate
        } else {
            llm_proxy_adapters::ClientWireFormat::OllamaChat
        },
        raw,
    );

    let is_streaming = core.stream;
    info!(
        request_id = %ctx.request_id,
        provider = %provider,
        model = %core.model.requested,
        streaming = is_streaming,
        endpoint = if generate { "/api/generate" } else { "/api/chat" },
        "decoded Ollama request into CoreRequest"
    );
    let inbound_auth = core_pipeline::extract_inbound_auth(&headers);

    let dispatch = core_pipeline::CoreDispatch {
        provider_name: &provider,
        route_kind: ProviderRouteKind::ChatCompletions,
        client_protocol: ClientProtocol::Ollama,
        ollama_generate: generate,
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
            ClientProtocol::Ollama.stream_wire(),
        )
        .await
    } else {
        core_pipeline::handle_core_once(state, ctx, dispatch, core, inbound).await
    }
}

/// `GET /providers/{provider}/api/tags`: the provider's (or pool's) model
/// catalog in the Ollama list shape.
pub(crate) async fn handle_ollama_tags(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    req: axum::extract::Request,
) -> Response<Body> {
    let start = std::time::Instant::now();
    let refresh_live = req
        .uri()
        .query()
        .is_some_and(|q| q.contains("refresh=live"));
    match handle_tags_inner(&state, &provider, refresh_live).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "tags request failed");
            core_pipeline::emit_response_failed(
                &state.event_bus,
                "tags",
                Some(provider.as_str()),
                None,
                &error,
                start,
            );
            route_error_response(ClientProtocol::Ollama, &error)
        }
    }
}

async fn handle_tags_inner(
    state: &AppState,
    provider_name: &str,
    refresh_live: bool,
) -> Result<Response<Body>, RouteError> {
    core_pipeline::validate_provider_name(provider_name).inspect_err(|e| {
        core_pipeline::emit_response_failed(
            &state.event_bus,
            "tags",
            Some(provider_name),
            None,
            e,
            std::time::Instant::now(),
        );
    })?;

    // A pool name lists the union of its members' catalogs, like /v1/models.
    let live_providers = state.providers();
    let member_names: Vec<String> = match live_providers.pool(provider_name) {
        Some(pool) => pool.members.clone(),
        None => {
            if live_providers.get(provider_name).is_none() {
                return Err(RouteError::UnknownProvider(provider_name.to_owned()));
            }
            vec![provider_name.to_owned()]
        }
    };

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for member in &member_names {
        let Some(provider) = live_providers.get(member) else {
            continue;
        };
        let entries = state
            .model_catalogs()
            .catalog(provider, refresh_live)
            .await
            .map_err(super::models::map_catalog_error)?;
        for entry in entries {
            if seen.insert(entry.id.clone()) {
                models.push(entry.id);
            }
        }
    }
    let body = llm_proxy_protocol::client::ollama::encode_tags_response(&models);
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}
