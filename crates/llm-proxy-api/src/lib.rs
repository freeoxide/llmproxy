//! Read-only, admin-authenticated `/usage` query surface over the proxy's
//! usage ledger: every route is a `GET` behind `Authorization: Bearer
//! <admin-token>` and no response carries request bodies.

#![deny(missing_docs)]

mod auth;
mod reader;
mod routes;

pub use routes::UsageApi;
