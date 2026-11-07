//! The axum wiring for `llm-proxy`: routes and error mapping only. The
//! request lifecycle lives in `llm-proxy-engine`; this crate binds it to
//! HTTP, decodes client dialects, and renders errors.

#![deny(missing_docs)]

/// Host/Origin validation under anonymous auth (DNS-rebinding defense).
pub mod host_guard;
/// Ingress self-loop refusal (508 on requests carrying the egress hop marker).
pub mod loop_guard;
/// Middleware components (auth, rate limiter, request ID, IP extraction).
pub mod middleware;
/// Route handlers and router composition.
pub mod routes;
/// Graceful shutdown signal.
pub mod shutdown;
/// Shared application state and build metadata.
pub mod state;

pub use middleware::{
    CallerId, ClientAuth, ClientAuthError, RateLimiter, RequestId, RequestIdGenerator,
};
pub use shutdown::shutdown_signal;
pub use state::{AppState, BuildInfo};

use axum::Router;

/// Build the application router for the given state.
#[must_use = "the Router must be used with a server"]
pub fn build_router(state: AppState) -> Router {
    routes::router(state)
}

/// Build the admin `/usage` router behind the `[auth.admin]` token.
///
/// Returns `None` when no `[auth.admin]` token or `[storage]` is configured
/// or the `admin` cargo feature is off; every route is a read-only `GET`.
/// With metrics attached the same surface serves `/metrics` (Prometheus
/// exposition, plan 07 step 7) behind the same token.
#[must_use = "the Router must be used with a server"]
pub fn admin_router(state: &AppState) -> Option<Router> {
    #[cfg(feature = "admin")]
    {
        use secrecy::ExposeSecret as _;
        let admin = state.app_config().auth.admin.as_ref()?;
        let storage = state.app_config().storage.as_ref()?;
        Some(
            llm_proxy_api::UsageApi::new(storage.path.clone(), admin.token.expose_secret())
                .with_metrics(std::sync::Arc::clone(state.metrics()))
                .router(),
        )
    }
    #[cfg(not(feature = "admin"))]
    {
        let _ = state;
        None
    }
}
