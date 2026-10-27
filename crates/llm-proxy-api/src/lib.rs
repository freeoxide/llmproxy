//! llm-proxy-api: the read-only, admin-authenticated `/usage` query surface
//! over the proxy's usage ledger.
//!
//! [`UsageApi`] builds a small axum router with three `GET` routes backed by
//! the SQLite ledger written by `llm-proxy-storage`'s `SqliteEventBus`:
//!
//! - `/usage/summary?window=1h&group_by=provider|model|caller` — token,
//!   request, error, and exact-cost totals grouped over a time window.
//! - `/usage/requests?since=…&until=…&provider=…&limit=…&cursor=…` —
//!   bounded, cursor-paged raw ledger rows. Never request bodies; the ledger
//!   stores only a body digest and so do these responses.
//! - `/usage/keys` — caller list with request counts and last-seen times.
//!
//! Every route requires `Authorization: Bearer <admin-token>`; the token is
//! compared in constant time against a digest and never appears in logs or
//! responses. All responses are strictly read-only: queries run on
//! read-only SQLite connections and there is no write path.

#![deny(missing_docs)]

mod auth;
mod reader;
mod routes;

pub use routes::UsageApi;
