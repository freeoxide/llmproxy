use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use crate::state::AppState;
#[derive(Serialize)]
pub(crate) struct HealthBody {
    status: &'static str,
    service: String,
}
pub(crate) async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthBody>) {
    (
        StatusCode::OK,
        Json(HealthBody {
            status: "ok",
            service: state.server_name().to_owned(),
        }),
    )
}
#[derive(Serialize)]
pub(crate) struct ReadyBody {
    status: &'static str,
}
pub(crate) async fn ready() -> (StatusCode, Json<ReadyBody>) {
    (StatusCode::OK, Json(ReadyBody { status: "ready" }))
}
#[derive(Serialize)]
pub(crate) struct VersionBody {
    name: String,
    version: &'static str,
    target: &'static str,
    git_sha: &'static str,
}
pub(crate) async fn version(State(state): State<AppState>) -> (StatusCode, Json<VersionBody>) {
    let build = state.build_info();
    (
        StatusCode::OK,
        Json(VersionBody {
            name: state.server_name().to_owned(),
            version: build.version,
            target: build.target,
            git_sha: build.git_sha,
        }),
    )
}
