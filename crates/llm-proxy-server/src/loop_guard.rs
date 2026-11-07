//! Ingress side of self-loop prevention (backlog 32): every upstream
//! request this proxy sends carries `x-llm-proxy-hop: 1` (stamped by the
//! transport). A request arriving here with that header already set is
//! inside a forwarding loop — this proxy (or a chained one) already sent it —
//! and is answered `508 Loop Detected` instead of being forwarded again.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;
use llm_proxy_core::HOP_HEADER;

/// Refuses requests that already carry this proxy's egress hop marker.
pub(crate) async fn refuse_looped_requests(
    State(_state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if req.headers().contains_key(HOP_HEADER) {
        tracing::error!(
            header = HOP_HEADER,
            "refusing a request that already carries this proxy's egress hop marker: \
             the upstream configuration loops back to a listener that forwards again"
        );
        return (
            StatusCode::LOOP_DETECTED,
            axum::Json(json!({
                "error": {
                    "type": "proxy_loop_detected",
                    "message": "refused: this request already passed through an llm-proxy \
                                egress hop (it carries the x-llm-proxy-hop marker). Fix the \
                                upstream configuration: no provider endpoint may resolve to a \
                                listener that forwards to this proxy again."
                }
            })),
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hop_header_name_matches_the_stamped_one() {
        assert_eq!(HOP_HEADER, "x-llm-proxy-hop");
    }
}
