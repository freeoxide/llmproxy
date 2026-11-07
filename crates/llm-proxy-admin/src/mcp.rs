//! The admin API as MCP tools (plan 09 step 3, backlog 35): one
//! Streamable HTTP endpoint at `POST /mcp` speaking protocol revision
//! [`MCP_PROTOCOL_VERSION`] (`2026-07-28`), behind the same admin bearer
//! token as the REST routes — the tools reuse the admin API's auth, they
//! do not add their own.
//!
//! Provenance: the framing follows the MCP Streamable HTTP transport
//! specification, revision 2026-07-28
//! (`https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http.md`):
//! every client message is one POST; requests are answered with a single
//! `application/json` JSON-RPC object (a server may always choose JSON
//! over SSE); notifications are acknowledged with `202 Accepted` and no
//! body; this revision has no protocol-level sessions, so none are minted
//! or echoed; the `MCP-Protocol-Version`, `Mcp-Method` and (for
//! `tools/call`) `Mcp-Name` headers are required and validated against
//! the body, with a `-32020` `HeaderMismatch` on disagreement; an unknown
//! method is `404` with `-32601`. `initialize` is answered for
//! older-era clients, per the revision's backward-compatibility note.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use crate::AdminSurface;
use crate::routes::AdminState;

/// The protocol revision this endpoint speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// JSON-RPC error codes used here.
const METHOD_NOT_FOUND: i64 = -32601;
const HEADER_MISMATCH: i64 = -32020;
const INVALID_REQUEST: i64 = -32600;

pub(crate) async fn endpoint(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let surface = state.surface;
    let Some(Json(message)) = body else {
        return error_no_id(
            StatusCode::BAD_REQUEST,
            INVALID_REQUEST,
            "the MCP endpoint needs one JSON-RPC message per POST",
        );
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return error_no_id(
            StatusCode::BAD_REQUEST,
            INVALID_REQUEST,
            "not a JSON-RPC message: no method",
        );
    };

    if let Some(rejection) = validate_headers(&headers, method) {
        return rejection;
    }
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) are acknowledged with 202 and no body.
    let id = message.get("id").cloned();
    let Some(id) = id else {
        if method.starts_with("notifications/") {
            return StatusCode::ACCEPTED.into_response();
        }
        return error_no_id(
            StatusCode::BAD_REQUEST,
            INVALID_REQUEST,
            "only JSON-RPC requests and notifications are accepted",
        );
    };

    let response = match method {
        // Version negotiation for initialization-era clients (see the
        // gateway crate for the interop finding): echo the revision the
        // client asked for, defaulting to this endpoint's own.
        "initialize" => {
            let negotiated = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": negotiated,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "llm-proxy-admin", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            match (
                params.get("name").and_then(Value::as_str),
                headers_mcp_name(&headers),
            ) {
                (Some(name), Some(header_name)) if name != header_name => {
                    return error_for_id(
                        StatusCode::BAD_REQUEST,
                        &id,
                        HEADER_MISMATCH,
                        "the Mcp-Name header does not match the request body",
                    );
                }
                (None, _) => {
                    return error_for_id(
                        StatusCode::BAD_REQUEST,
                        &id,
                        INVALID_REQUEST,
                        "tools/call needs a tool name",
                    );
                }
                _ => {}
            }
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(&surface, name, &arguments).await
        }
        other => {
            return error_for_id(
                StatusCode::NOT_FOUND,
                &id,
                METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            );
        }
    };

    match response {
        Ok(result) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        )
            .into_response(),
        Err(message) => error_for_id(StatusCode::OK, &id, METHOD_NOT_FOUND, &message),
    }
}

/// Validates the mirrored request headers against the body (spec: Server
/// Validation): the protocol-version header must be present and name the
/// revision this endpoint speaks, and `Mcp-Method` must match the body's
/// method. Returns the rejection response on failure.
fn validate_headers(headers: &HeaderMap, method: &str) -> Option<Response> {
    let version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());
    match version {
        Some(MCP_PROTOCOL_VERSION) => {}
        Some(other) => {
            return Some(error_no_id(
                StatusCode::BAD_REQUEST,
                HEADER_MISMATCH,
                &format!(
                    "unsupported protocol version {other}; this endpoint speaks \
                     {MCP_PROTOCOL_VERSION}"
                ),
            ));
        }
        None => {
            return Some(error_no_id(
                StatusCode::BAD_REQUEST,
                HEADER_MISMATCH,
                "the MCP-Protocol-Version header is required",
            ));
        }
    }
    if let Some(header_method) = headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        && header_method != method
    {
        return Some(error_no_id(
            StatusCode::BAD_REQUEST,
            HEADER_MISMATCH,
            "the Mcp-Method header does not match the request body",
        ));
    }
    None
}

fn headers_mcp_name(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("mcp-name")
        .and_then(|value| value.to_str().ok())
}

fn error_no_id(status: StatusCode, code: i64, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
        })),
    )
        .into_response()
}

fn error_for_id(status: StatusCode, id: &Value, code: i64, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })),
    )
        .into_response()
}

/// The tool catalogue: the admin API surface, one tool per operation.
fn tools() -> Value {
    json!([
        tool(
            "usage_summary",
            "Usage totals grouped by provider, model or caller over a window.",
            &json!({
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "humantime window like \"24h\"" },
                    "group_by": { "type": "string", "enum": ["provider", "model", "caller"] }
                },
            }),
        ),
        tool(
            "list_keys",
            "List the managed client keys (never their verifiers or secrets).",
            &json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "create_key",
            "Issue a new client key. The plaintext token is returned once.",
            &json!({
                "type": "object",
                "properties": {
                    "allow_models": { "type": "array", "items": { "type": "string" } },
                    "allow_providers": { "type": "array", "items": { "type": "string" } },
                    "rpm": { "type": "integer" },
                    "tpm": { "type": "integer" },
                    "rpd": { "type": "integer" },
                    "usd_per_day": { "type": "string", "description": "fixed-point USD like \"5.00\"" },
                },
            }),
        ),
        tool(
            "revoke_key",
            "Revoke a managed client key by id.",
            &json!({
                "type": "object",
                "properties": { "key_id": { "type": "string" } },
                "required": ["key_id"],
            }),
        ),
        tool(
            "get_config",
            "The running proxy configuration with every secret redacted.",
            &json!({ "type": "object", "properties": {} }),
        ),
    ])
}

fn tool(name: &str, description: &str, input_schema: &Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

/// Dispatches one tool call to the same handlers the REST routes use.
async fn call_tool(surface: &AdminSurface, name: &str, arguments: &Value) -> Result<Value, String> {
    let payload = match name {
        "list_keys" => {
            let keys = crate::routes::list_keys_json(surface)?;
            json!({ "keys": keys })
        }
        "create_key" => {
            let policy: crate::KeyPolicyInput = serde_json::from_value(arguments.clone())
                .map_err(|error| format!("invalid key policy: {error}"))?;
            let issued = crate::routes::create_key_json(surface, &policy)?;
            json!({
                "key_id": issued.key_id,
                "token": issued.token.expose_secret(),
                "created_at": issued.created_at,
                "note": "the token is shown once; store it now",
            })
        }
        "revoke_key" => {
            let key_id = arguments
                .get("key_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "revoke_key needs key_id".to_owned())?;
            crate::routes::revoke_key_json(surface, key_id)?;
            json!({ "revoked": key_id })
        }
        "get_config" => crate::config_view::sanitize_config(
            surface
                .config_ref()
                .ok_or_else(|| "no config was attached to this surface".to_owned())?,
        ),
        "usage_summary" => {
            crate::routes::usage_summary_json(
                surface,
                arguments
                    .get("window")
                    .and_then(Value::as_str)
                    .unwrap_or("24h"),
                arguments
                    .get("group_by")
                    .and_then(Value::as_str)
                    .unwrap_or("provider"),
            )
            .await?
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    Ok(json!({
        "content": [ { "type": "text", "text": payload.to_string() } ],
        "isError": false,
    }))
}
