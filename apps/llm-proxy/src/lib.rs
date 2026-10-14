//! `llm-proxy` application library: a multi-command CLI for the LLM proxy
//! server, using TOML config exclusively.

pub mod cli;
pub mod commands;
pub mod config_validation;
pub mod defaults;
pub mod paths;
pub mod permissions;
pub mod pid;
pub mod platform;
pub mod state;
