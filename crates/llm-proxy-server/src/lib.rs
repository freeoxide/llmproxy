//! The axum wiring for `llm-proxy`.

#![deny(missing_docs)]

/// Runtime provider model catalog cache and refresh service.
pub mod catalog_service;
/// Middleware components (dedup, rate limiter, request ID, IP extraction).
pub mod middleware;
/// Route handlers and router composition.
pub mod routes;
/// Graceful shutdown signal.
pub mod shutdown;
/// Shared application state and build metadata.
pub mod state;

pub use catalog_service::{ModelCatalogService, write_catalog_atomic};
pub use middleware::{
    CallerId, ClientAuth, ClientAuthError, RateLimiter, RequestDeduplicator, RequestId,
    RequestIdGenerator,
};
pub use shutdown::shutdown_signal;
pub use state::{AppState, BuildInfo};

use axum::Router;
use secrecy::ExposeSecret;

/// Build the application router for the given state.
#[must_use = "the Router must be used with a server"]
pub fn build_router(state: AppState) -> Router {
    routes::router(state)
}

/// Build the admin `/usage` router behind the `[auth.admin]` token.
///
/// Returns `None` when no `[auth.admin]` token or `[storage]` is configured;
/// every route is a read-only `GET`.
#[must_use = "the Router must be used with a server"]
pub fn admin_router(state: &AppState) -> Option<Router> {
    let admin = state.app_config.auth.admin.as_ref()?;
    let storage = state.app_config.storage.as_ref()?;
    Some(llm_proxy_api::UsageApi::new(storage.path.clone(), admin.token.expose_secret()).router())
}
