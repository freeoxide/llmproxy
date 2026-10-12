//! The axum wiring for `llm-proxy`.
//!
//! Exposes [`build_router`] which assembles the full HTTP router
//! with middleware, routes, and shared state.

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

/// Build the application router for the given state.
///
/// Carries an explicit `#[must_use]` (audit LOW-2): `axum::Router` is not
/// itself `#[must_use]`, so this attribute is the single, non-redundant
/// reminder that the returned router must be driven by a server. No
/// `clippy::double_must_use` annotation is warranted — that lint does not fire
/// here (verified: an `#[expect(clippy::double_must_use)]` was unfulfilled),
/// because only the function, not the return type, is `#[must_use]`. The
/// former inert `#[allow(...)]` that LOW-2 flagged has been removed.
#[must_use = "the Router must be used with a server"]
pub fn build_router(state: AppState) -> Router {
    routes::router(state)
}
