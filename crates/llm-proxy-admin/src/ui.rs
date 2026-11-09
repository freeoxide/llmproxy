//! The embedded web UI: one static page that calls the JSON API with the
//! operator's admin token (kept in the browser only). No build step, no
//! external assets, no server-side templating — the API is the contract.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Serves the UI page at `/`.
pub(crate) async fn index() -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        include_str!("../assets/index.html"),
    )
        .into_response()
}
