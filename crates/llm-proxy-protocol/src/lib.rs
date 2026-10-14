//! llm-proxy-protocol: wire-format types for the LLM proxy — Anthropic
//! Messages, OpenAI Chat Completions, Responses API, Gemini API, and the
//! normalised core protocol types.

#![deny(missing_docs)]

pub mod anthropic;
pub mod client;
pub mod core;
pub mod openai;
pub mod util;
pub mod zen;
