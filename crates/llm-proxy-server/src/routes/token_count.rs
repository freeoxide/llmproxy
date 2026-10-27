use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response};
use axum::response::IntoResponse;
use llm_proxy_core::MessageContent;
use llm_proxy_protocol::anthropic::MessageRequest;
use llm_proxy_protocol::client::anthropic;
use llm_proxy_protocol::core::{StopReason, Usage};
use llm_proxy_storage::{ProxyEvent, RequestReceived, ResponseCompleted};
use serde::Serialize;

use crate::middleware::RequestMeta;
use crate::state::AppState;

use super::core_pipeline;
use super::error_response::{ClientProtocol, RouteError, route_error_response};
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenCountResponse {
    input_tokens: usize,
}
pub(crate) async fn count_tokens(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    match count_tokens_inner(&state, meta, &provider, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Anthropic, &error)
        }
    }
}
fn json_rejection_message(rejection: &JsonRejection) -> String {
    match rejection {
        JsonRejection::JsonSyntaxError(e) => format!("invalid JSON syntax: {e}"),
        JsonRejection::JsonDataError(e) => format!("JSON body did not match expected type: {e}"),
        JsonRejection::MissingJsonContentType(_) => {
            "missing application/json Content-Type".to_owned()
        }
        other => format!("could not read request body: {other}"),
    }
}
fn extract_countable_text(
    core: &llm_proxy_protocol::core::CoreRequest,
) -> (String, Vec<MessageContent>) {
    let system_text: String = core
        .system
        .iter()
        .filter_map(|block| match block {
            llm_proxy_protocol::core::CoreContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .fold(String::new(), |mut acc, s| {
            acc.push_str(s);
            acc
        });

    let messages: Vec<MessageContent> = core
        .messages
        .iter()
        .map(|msg| {
            let text: String = msg
                .content
                .iter()
                .filter_map(|block| match block {
                    llm_proxy_protocol::core::CoreContent::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if text.is_empty() && !msg.content.is_empty() {
                tracing::debug!(
                    role = ?msg.role,
                    blocks = msg.content.len(),
                    "message has content blocks but no extractable text; token estimate will be zero for this message"
                );
            }
            let role_name = match msg.role {
                llm_proxy_protocol::core::CoreRole::User => "user",
                llm_proxy_protocol::core::CoreRole::Assistant => "assistant",
                llm_proxy_protocol::core::CoreRole::System => "system",
                llm_proxy_protocol::core::CoreRole::Tool => {
                    tracing::debug!(
                        "CoreRole::Tool mapped to 'user' for token counting; \
                         tool_result content is counted under user role"
                    );
                    "user"
                }
                _ => {
                    tracing::warn!(
                        role = ?msg.role,
                        "unknown CoreRole variant in token count, mapping to 'user'"
                    );
                    "user"
                }
            };
            MessageContent::new(role_name, text)
        })
        .collect();

    (system_text, messages)
}

async fn count_tokens_inner(
    state: &AppState,
    meta: RequestMeta,
    provider: &str,
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response<Body>, RouteError> {
    let start = std::time::Instant::now();
    let event_bus = Arc::clone(&state.event_bus);
    let request_id = meta.request_id.0.clone();
    let emit_failed = |e: &RouteError| {
        core_pipeline::emit_response_failed(
            &event_bus,
            &request_id,
            Some(provider),
            None,
            e,
            start,
        );
    };
    core_pipeline::validate_provider_name(provider).inspect_err(&emit_failed)?;
    if state.providers().get(provider).is_none() {
        let err = RouteError::UnknownProvider(provider.to_owned());
        emit_failed(&err);
        return Err(err);
    }
    core_pipeline::validate_json_content_type(headers).inspect_err(&emit_failed)?;
    let request_path = format!("/providers/{provider}/v1/messages/count_tokens");
    let ctx = core_pipeline::prepare_request(state, &meta, headers, &body, &request_path)
        .inspect_err(&emit_failed)?;
    let req: MessageRequest = match axum::Json::<MessageRequest>::from_bytes(&body) {
        Ok(axum::Json(value)) => value,
        Err(rejection) => {
            let err = RouteError::InvalidRequest(json_rejection_message(&rejection));
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
    state
        .event_bus
        .emit(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: ctx.caller.clone(),
            provider: provider.to_owned(),
            route_kind: "count_tokens".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: core.model.clone(),
            streaming: false,
            body_hash: super::core_pipeline::body_hash_of_bytes(&body),
        }));
    let (system_text, messages) = extract_countable_text(&core);
    let counter = state.token_counter.clone();
    let model = core.model.requested.clone();
    let count = tokio::task::spawn_blocking(move || {
        counter.count_messages(&model, &system_text, &messages)
    })
    .await
    .map_err(|e| RouteError::Internal(format!("token count task failed: {e}")))?;
    let latency = start.elapsed();
    state
        .event_bus
        .emit(&ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: provider.to_owned(),
            upstream_message_id: None,
            model: core.model.clone(),
            usage: Usage::synthetic_zero(),
            cost: None,
            stop_reason: StopReason::EndTurn,
            latency_ms: latency.as_millis().try_into().unwrap_or(u64::MAX),
        }));

    let response = axum::Json(TokenCountResponse {
        input_tokens: count,
    });

    let mut response = response.into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );

    response.headers_mut().insert(
        "x-request-id",
        ctx.request_id
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")),
    );

    Ok(response)
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_guard_token_count_uses_current_architecture() {
        let source = include_str!("token_count.rs");
        let prod = source
            .split_once("#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(source);

        assert!(
            !prod.contains("ApiError"),
            "token_count.rs must use ServerError"
        );
        assert!(
            !prod.contains("crate::error::"),
            "token_count.rs must not import from crate::error"
        );
        assert!(
            !prod.contains("content_blocks()"),
            "token_count.rs must not use removed content_blocks() method"
        );
        assert!(
            !prod.contains("system_text()"),
            "token_count.rs must not use removed system_text() method"
        );
    }
}
