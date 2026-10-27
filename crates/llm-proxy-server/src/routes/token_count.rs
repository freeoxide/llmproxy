//! `/providers/{provider}/v1/messages/count_tokens` handler.
//!
//! Accepts an Anthropic-format request, decodes it through the Anthropic client
//! adapter into a `CoreRequest`, and estimates the token count from the core
//! representation.

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

/// Response body for the token count endpoint.
///
/// **Note:** The `input_tokens` value is computed locally with the model's own
/// BPE tokenizer for known OpenAI models (`gpt-4o`, `gpt-4`, `gpt-3.5`, etc.),
/// and falls back to a character-level heuristic (~4 chars/token) for any model
/// the proxy does not have a tokenizer for. It is therefore exact for known
/// OpenAI text but still an estimate for other providers and for non-text
/// content. It intentionally excludes tool definitions, non-text content blocks
/// (images, documents), and tool-result content. For non-OpenAI providers the
/// provider-reported usage (returned in the actual response) remains the
/// authoritative count for billing; this estimate is for pre-flight size gating.
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenCountResponse {
    input_tokens: usize,
}

/// POST `/providers/{provider}/v1/messages/count_tokens`
///
/// Accepts an Anthropic-format request, decodes it through the Anthropic client
/// adapter into a `CoreRequest`, and estimates the token count using the
/// configured tokenizer: the model's own BPE tokenizer for known OpenAI models,
/// falling back to the ~4 chars/token heuristic otherwise.
///
/// This handler uses the core pipeline decode (`anthropic::decode_request`)
/// rather than direct field access on `MessageRequest`, so the requested model
/// id (`core.model.requested`) is available to select the right tokenizer.
pub(crate) async fn count_tokens(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    // The wrapper only renders: it does NOT emit ResponseFailed. token_count
    // has no core-pipeline dispatch, so each early-gate error return inside the
    // inner function emits exactly one ResponseFailed itself, and the success
    // path emits exactly one ResponseCompleted. A blanket emit here would
    // double-count, so the wrapper stays emit-free
    // (audit route-responsefailed-gaps regression).
    match count_tokens_inner(&state, meta, &provider, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "request failed");
            route_error_response(ClientProtocol::Anthropic, &error)
        }
    }
}

/// Render an axum [`JsonRejection`] as a human-readable message, preserving
/// the distinction between a syntactically invalid JSON body and a body that is
/// valid JSON but does not fit the target type (audit LOW-28).
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

/// Extract the text a token estimate counts from a decoded [`CoreRequest`]:
/// the concatenated system text and one [`MessageContent`] per message.
///
/// NOTE: This estimate intentionally only counts text from `CoreContent::Text`
/// blocks. The following are NOT counted:
///   - Tool definitions (`core.tools`) -- their name, description, and
///     input_schema text fields are excluded.
///   - Non-text content blocks (images, documents, etc.).
///   - Tool result content blocks.
///   - System prompt non-text blocks.
///
/// TODO(future): Extend counting to include tool definitions, tool_use blocks,
/// and tool_result blocks for a more accurate estimate.
fn extract_countable_text(
    core: &llm_proxy_protocol::core::CoreRequest,
) -> (String, Vec<MessageContent>) {
    // Collect system text without intermediate Vec allocation.
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
            // Non-text content blocks (images, tool_use, thinking) produce
            // empty text here. This is a known limitation of the heuristic
            // counter. Log at debug level so operators can detect requests
            // where the estimate may be significantly off.
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
                // The Anthropic Messages API does not have a 'tool' role.
                // Tool results are carried in 'user' role messages with
                // tool_result content blocks. If a CoreRole::Tool reaches
                // here, treat it as 'user' since tool_result content is
                // typically nested under the user role in Anthropic.
                llm_proxy_protocol::core::CoreRole::Tool => {
                    tracing::debug!(
                        "CoreRole::Tool mapped to 'user' for token counting; \
                         tool_result content is counted under user role"
                    );
                    "user"
                }
                // All known variants are handled above. This catch-all exists
                // for forward compatibility. Unknown roles are treated as
                // 'user' since that is the least-lossy mapping.
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
    // Pre-flight: rate limit, dedup, request ID. token_count has no core-pipeline
    // dispatch, so each EARLY-GATE error return below emits exactly one
    // ResponseFailed itself, and the success path emits exactly one
    // ResponseCompleted (audit route-responsefailed-gaps).
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
    // Reject non-JSON Content-Type before parsing, so a wrong media type is not
    // misreported as a JSON syntax error (audit LOW-30).
    core_pipeline::validate_json_content_type(headers).inspect_err(&emit_failed)?;
    let request_path = format!("/providers/{provider}/v1/messages/count_tokens");
    let ctx = core_pipeline::prepare_request(state, &meta, headers, &body, &request_path)
        .inspect_err(&emit_failed)?;
    // Parse and validate the Anthropic MessageRequest.
    //
    // Go through `axum::Json::from_bytes` (rather than `serde_json::from_slice`)
    // so the `JsonRejection` taxonomy is preserved: syntax errors and data
    // (deserialization) errors are surfaced with distinct messages instead of
    // being collapsed into a single opaque "invalid JSON" string.
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

    // Decode through the Anthropic client adapter to get a CoreRequest.
    // This validates the request shape and normalises it.
    let core = anthropic::decode_request(req)
        .map_err(core_pipeline::protocol_error_to_route)
        .inspect_err(&emit_failed)?;

    // Emit a RequestReceived event for the token-count request. token_count is a
    // real proxy request (it decodes client input and consumes rate-limit/dedup
    // budget) so it is logged like any other route. route_kind/client_protocol
    // are marked "count_tokens" / "anthropic" so consumers can distinguish it
    // from inference requests (audit eventlog-early-gates).
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

    // Extract text content from core messages for token counting.
    let (system_text, messages) = extract_countable_text(&core);

    // BPE tokenization is CPU-bound (fancy-regex driven) and the request body
    // is attacker-controlled up to MAX_BODY_BYTES (32 MiB). Running it on the
    // tokio worker thread stalls every other future polled on that worker for
    // the full duration of the encode, so offload it to the blocking pool.
    // `Counter::clone` is a cheap refcount bump; `system_text` and `messages`
    // are already owned here and are the last use on this path.
    let counter = state.token_counter.clone();
    let model = core.model.requested.clone();
    let count = tokio::task::spawn_blocking(move || {
        counter.count_messages(&model, &system_text, &messages)
    })
    .await
    .map_err(|e| RouteError::Internal(format!("token count task failed: {e}")))?;

    // Emit a ResponseCompleted event. token_count performs no upstream inference
    // so there is no provider usage, cost, or stop reason; usage is a synthetic
    // zero with provenance SyntheticZero and stop_reason is EndTurn so the event
    // shape stays uniform with the inference routes. The computed token estimate
    // travels in the HTTP response body, not the event (audit eventlog-early-gates).
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

    // Defense-in-depth: explicitly set Content-Type even though
    // axum::Json's IntoResponse already sets it internally.
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

// ===========================================================================
// Tests
// ===========================================================================

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
