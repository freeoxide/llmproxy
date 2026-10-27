//! Axum handlers and router for the `/usage` query surface.

use std::path::PathBuf;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde::Serialize;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{AdminAuth, require_admin};
use crate::reader::{GroupBy, MAX_PAGE_LIMIT, RequestFilter, UsageReader, now_unix_millis};

/// Default summary window when `?window=` is absent: 24 hours.
const DEFAULT_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Shared state for the `/usage` routes: the ledger reader and the admin
/// token guard.
#[derive(Clone, Debug)]
pub(crate) struct UsageState {
    pub(crate) reader: UsageReader,
    pub(crate) admin: AdminAuth,
}

/// The `/usage` query surface: a read-only, admin-authenticated router.
///
/// Build one with [`UsageApi::new`] and either serve it on its own listener
/// (the dedicated admin listener) or merge it into another router (the
/// opt-in main-listener mount). There is no write path: every route is a
/// `GET` backed by read-only SQLite connections.
#[derive(Debug)]
pub struct UsageApi {
    state: UsageState,
}

impl UsageApi {
    /// Build the `/usage` surface over the ledger at `db_path`, guarded by
    /// `admin_token`.
    #[must_use]
    pub fn new(db_path: impl Into<PathBuf>, admin_token: &str) -> Self {
        Self {
            state: UsageState {
                reader: UsageReader::new(db_path),
                admin: AdminAuth::new(admin_token),
            },
        }
    }

    /// The `/usage` router with state applied, ready to serve or merge.
    ///
    /// Every route sits behind the admin-token middleware. The outer layers
    /// (`nosniff`, panic catching, tracing) are installed here so the same
    /// protections apply on both the dedicated listener and any merged
    /// mount, independent of the surrounding router's own layers.
    #[must_use = "the Router must be used with a server or merged into one"]
    pub fn router(self) -> axum::Router {
        let state = self.state;
        axum::Router::new()
            .route("/usage/summary", get(summary))
            .route("/usage/requests", get(requests))
            .route("/usage/keys", get(keys))
            .layer(from_fn_with_state(state.clone(), require_admin))
            .layer(TraceLayer::new_for_http())
            .layer(SetResponseHeaderLayer::if_not_present(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .layer(CatchPanicLayer::new())
            .with_state(state)
    }
}

/// Handler error: invalid query parameters (400) or a ledger failure (500,
/// with the detail logged server-side only).
#[derive(Debug)]
pub(crate) enum UsageApiError {
    /// A query parameter failed validation; the message is safe to return.
    InvalidParam(String),
    /// The ledger query failed; the response carries a generic message.
    Query(crate::reader::UsageQueryError),
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    r#type: &'static str,
    message: String,
}

impl IntoResponse for UsageApiError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidParam(message) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: ErrorDetail {
                        r#type: "invalid_request",
                        message,
                    },
                }),
            )
                .into_response(),
            Self::Query(error) => {
                tracing::error!(error = %error, "usage ledger query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorBody {
                        error: ErrorDetail {
                            r#type: "internal_error",
                            message: "usage ledger query failed".to_owned(),
                        },
                    }),
                )
                    .into_response()
            }
        }
    }
}

#[derive(Deserialize)]
struct SummaryParams {
    window: Option<String>,
    group_by: Option<String>,
}

#[derive(Serialize)]
struct SummaryBody {
    since_ms: i64,
    until_ms: i64,
    group_by: &'static str,
    groups: Vec<crate::reader::SummaryGroup>,
}

/// `GET /usage/summary?window=1h&group_by=provider|model|caller`.
async fn summary(
    State(state): State<UsageState>,
    Query(params): Query<SummaryParams>,
) -> Result<Json<SummaryBody>, UsageApiError> {
    let until_ms = now_unix_millis();
    let window_ms = match &params.window {
        None => DEFAULT_WINDOW.as_millis(),
        Some(raw) => {
            let parsed = humantime::parse_duration(raw).map_err(|_parse_error| {
                UsageApiError::InvalidParam(format!(
                    "window must be a duration like \"1h\" or \"30m\", got {raw:?}"
                ))
            })?;
            if parsed.is_zero() {
                return Err(UsageApiError::InvalidParam(
                    "window must be a positive duration".to_owned(),
                ));
            }
            parsed.as_millis()
        }
    };
    let since_ms = until_ms.saturating_sub(i64::try_from(window_ms).unwrap_or(i64::MAX));
    let group = match params.group_by.as_deref() {
        None | Some("provider") => GroupBy::Provider,
        Some(other) => GroupBy::parse(other).ok_or_else(|| {
            UsageApiError::InvalidParam(format!(
                "group_by must be one of provider, model, caller; got {other:?}"
            ))
        })?,
    };
    let groups = state
        .reader
        .summary(since_ms, group)
        .await
        .map_err(UsageApiError::Query)?;
    Ok(Json(SummaryBody {
        since_ms,
        until_ms,
        group_by: group.as_str(),
        groups,
    }))
}

#[derive(Deserialize)]
struct RequestsParams {
    since: Option<String>,
    until: Option<String>,
    provider: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

#[derive(Serialize)]
struct RequestsBody {
    rows: Vec<crate::reader::RequestRow>,
    next_cursor: Option<String>,
}

/// Parse an RFC 3339 timestamp parameter into Unix epoch milliseconds.
fn parse_rfc3339_ms(name: &str, raw: &str) -> Result<i64, UsageApiError> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(raw, &Rfc3339)
        .map(|ts| {
            ts.unix_timestamp()
                .saturating_mul(1000)
                .saturating_add(i64::from(ts.millisecond()))
        })
        .map_err(|_parse_error| {
            UsageApiError::InvalidParam(format!(
                "{name} must be an RFC 3339 timestamp like 2026-01-02T03:04:05Z, got {raw:?}"
            ))
        })
}

/// Parse an opaque `cursor` (`<timestamp_ms>:<request_id>`).
fn parse_cursor(raw: &str) -> Result<(i64, String), UsageApiError> {
    let Some((ts, id)) = raw.split_once(':') else {
        return Err(UsageApiError::InvalidParam(
            "cursor is malformed; pass back the next_cursor value unchanged".to_owned(),
        ));
    };
    let ts: i64 = ts.parse().map_err(|_parse_error| {
        UsageApiError::InvalidParam(
            "cursor is malformed; pass back the next_cursor value unchanged".to_owned(),
        )
    })?;
    if id.is_empty() {
        return Err(UsageApiError::InvalidParam(
            "cursor is malformed; pass back the next_cursor value unchanged".to_owned(),
        ));
    }
    Ok((ts, id.to_owned()))
}

/// `GET /usage/requests?since=…&until=…&provider=…&limit=…&cursor=…`.
///
/// Rows come back newest first, bounded by `limit` (default 100, maximum
/// 1000), paged via the returned `next_cursor`. `since` is inclusive,
/// `until` exclusive; both are RFC 3339. Responses never contain request
/// bodies — the ledger stores only the body digest.
async fn requests(
    State(state): State<UsageState>,
    Query(params): Query<RequestsParams>,
) -> Result<Json<RequestsBody>, UsageApiError> {
    let since_ms = match &params.since {
        Some(raw) => Some(parse_rfc3339_ms("since", raw)?),
        None => None,
    };
    let until_ms = match &params.until {
        Some(raw) => Some(parse_rfc3339_ms("until", raw)?),
        None => None,
    };
    if let (Some(since), Some(until)) = (since_ms, until_ms)
        && since >= until
    {
        return Err(UsageApiError::InvalidParam(
            "since must be before until".to_owned(),
        ));
    }
    let limit = params.limit.unwrap_or(100);
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(UsageApiError::InvalidParam(format!(
            "limit must be between 1 and {MAX_PAGE_LIMIT}, got {limit}"
        )));
    }
    let cursor = match &params.cursor {
        Some(raw) => Some(parse_cursor(raw)?),
        None => None,
    };
    let page = state
        .reader
        .requests(RequestFilter {
            since_ms,
            until_ms,
            provider: params.provider,
            limit,
            cursor,
        })
        .await
        .map_err(UsageApiError::Query)?;
    Ok(Json(RequestsBody {
        rows: page.rows,
        next_cursor: page.next_cursor,
    }))
}

#[derive(Serialize)]
struct KeysBody {
    callers: Vec<crate::reader::CallerSummary>,
}

/// `GET /usage/keys` — caller list with request counts, error counts, and
/// last-seen times.
async fn keys(State(state): State<UsageState>) -> Result<Json<KeysBody>, UsageApiError> {
    let callers = state.reader.callers().await.map_err(UsageApiError::Query)?;
    Ok(Json(KeysBody { callers }))
}
