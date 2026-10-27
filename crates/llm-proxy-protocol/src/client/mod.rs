//! Client protocol adapters: each translates between one client wire format
//! and the normalised core protocol types, never another adapter's format.

pub mod anthropic;
pub mod openai_chat;

/// Errors returned by client protocol adapters. Route handlers map
/// `InvalidRequest`/`Decode` to 400 and `Encode` to 500 (502 for
/// unrepresentable upstream data); no status is carried, so context wins.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The client request could not be decoded into core types.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// An error occurred while encoding core types to the client protocol.
    #[error("encode error: {0}")]
    Encode(String),
    /// An error occurred while decoding the client protocol into core types.
    #[error("decode error: {0}")]
    Decode(String),
    /// A content block was dropped because the client protocol does not support
    /// it, but the omission is safe (e.g. Document/Audio/Video in Anthropic
    /// responses). The block was already logged with a `tracing::warn!`.
    #[error("skippable encode: {0}")]
    EncodeSkippable(String),
}
