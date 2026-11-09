//! `llm-proxy` application library: a multi-command CLI for the LLM proxy
//! server, using TOML config exclusively.

pub mod cli;
pub mod commands;
pub mod config_validation;
pub mod defaults;
pub mod otel_metrics;
pub mod paths;
pub mod permissions;
pub mod pid;
pub mod platform;
pub mod provenance;
pub mod reload;
/// Replica-store wiring (plan 09 step 1): `[replica]` moves admission,
/// cooldowns and usage to the shared store.
pub mod replica;
pub mod state;
