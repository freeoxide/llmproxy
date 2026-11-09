//! Shared accounting for the gateway (plan 09 step 6): every `tools/call`
//! reserves through the proxy's [`Admission`] step and settles under the
//! same rules, and every call writes the same [`ProxyEvent`] usage rows
//! the chat pipeline writes — `route_kind: "mcp"`.

use llm_proxy_engine::{Admission, AdmissionClaim, AdmissionOutcome, AdmissionTicket, EngineError};
use llm_proxy_protocol::core::{Cost, ModelRef, StopReason, Usage, UsageProvenance};
use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed};
use sha2::{Digest as Sha256Digest, Sha256};
use time::OffsetDateTime;

/// What one tool call knows when it starts.
pub(crate) struct ToolCall<'a> {
    pub(crate) caller: &'a str,
    pub(crate) server: &'a str,
    pub(crate) tool: &'a str,
    pub(crate) params: &'a serde_json::Value,
}

/// Reserves admission for one tool call. `Ok(None)` means the configured
/// admission step is the allow-all default (nothing to reserve, nothing
/// to settle) — the same shortcut the chat pipeline's noop detection
/// takes.
pub(crate) async fn reserve(
    admission: &dyn Admission,
    call: &ToolCall<'_>,
) -> Result<Option<AdmissionTicket>, EngineError> {
    let body = serde_json::to_vec(call.params).unwrap_or_default();
    let body_hash = format!("{:x}", Sha256::digest(&body));
    let claim = AdmissionClaim {
        caller: call.caller,
        provider: &format!("mcp:{}", call.server),
        member: &format!("mcp:{}", call.server),
        model: call.tool,
        requested_model: call.tool,
        body_hash: &body_hash,
        // Tool arguments are not chat tokens; the estimate is the JSON
        // length in bytes / 4, the same offline heuristic non-OpenAI
        // models get (plan 05 step 8).
        estimated_input_tokens: (body.len() as u64) / 4,
        max_output_tokens: None,
    };
    admission.reserve(&claim).await.map(Some)
}

/// Settles one tool call: the reservation is released, and a failed call
/// charges nothing (no usage); a successful one reports zero token usage
/// (MCP tool calls have none) with the row recording the call itself.
pub(crate) fn settle(admission: &dyn Admission, ticket: &Option<AdmissionTicket>, succeeded: bool) {
    let Some(ticket) = ticket else {
        return;
    };
    admission.settle(
        ticket,
        &AdmissionOutcome {
            succeeded,
            usage: None,
            cost: None,
        },
    );
}

/// Emits the request row for one tool call (`route_kind: "mcp"`).
pub(crate) fn emit_request(bus: &dyn EventBus, request_id: &str, call: &ToolCall<'_>) {
    let body = serde_json::to_vec(call.params).unwrap_or_default();
    bus.emit(&ProxyEvent::RequestReceived(RequestReceived {
        request_id: request_id.to_owned(),
        timestamp: OffsetDateTime::now_utc(),
        caller: call.caller.to_owned(),
        provider: format!("mcp:{}", call.server),
        route_kind: "mcp".to_owned(),
        client_protocol: "mcp".to_owned(),
        model: ModelRef {
            requested: call.tool.to_owned(),
            upstream: None,
        },
        streaming: false,
        body_hash: format!("{:x}", Sha256::digest(&body)),
        session_id: None,
    }));
}

/// Emits the completion row for one tool call.
pub(crate) fn emit_completed(
    bus: &dyn EventBus,
    request_id: &str,
    call: &ToolCall<'_>,
    latency_ms: u64,
) {
    bus.emit(&ProxyEvent::ResponseCompleted(ResponseCompleted {
        request_id: request_id.to_owned(),
        timestamp: OffsetDateTime::now_utc(),
        provider: format!("mcp:{}", call.server),
        upstream_message_id: None,
        model: ModelRef {
            requested: call.tool.to_owned(),
            upstream: None,
        },
        // MCP tool calls carry no token usage; the row records that
        // explicitly rather than omitting the call.
        usage: Usage {
            provenance: UsageProvenance::SyntheticZero,
            ..Usage::default()
        },
        cost: Some(Cost::default()),
        stop_reason: StopReason::EndTurn,
        latency_ms,
        credential_id: None,
        cache_hit: false,
    }));
}

/// Emits the failure row for one tool call.
pub(crate) fn emit_failed(
    bus: &dyn EventBus,
    request_id: &str,
    call: &ToolCall<'_>,
    error: &str,
    status: u16,
    latency_ms: u64,
) {
    bus.emit(&ProxyEvent::ResponseFailed(ResponseFailed {
        request_id: request_id.to_owned(),
        timestamp: OffsetDateTime::now_utc(),
        provider: Some(format!("mcp:{}", call.server)),
        model: Some(ModelRef {
            requested: call.tool.to_owned(),
            upstream: None,
        }),
        error_kind: "McpToolCallFailed".to_owned(),
        message: error.to_owned(),
        http_status: status,
        latency_ms,
    }));
}
