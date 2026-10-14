//! OpenAI Chat Completions API types for `/v1/chat/completions`, including
//! streaming chunks and error payloads.
//! Reference: <https://platform.openai.com/docs/api-reference/chat>

use serde::{Deserialize, Serialize};

/// Cache-control directive attached to a message or content block; distinct
/// from [`crate::core::CacheControl`] because the OpenAI wire `type` field is
/// a plain string rather than a typed enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    /// Discriminator – typically `"ephemeral"`.
    pub r#type: String,
}

impl CacheControl {
    /// The known cache-control type value used by Anthropic.
    pub const EPHEMERAL: &str = "ephemeral";
}

/// Controls extra metadata returned alongside streaming chunks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// When `true`, the final chunk includes a [`UsageInfo`] object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

/// A tool (function) that the model may invoke during generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Always `"function"` for function-calling tools; kept as a bare `String`
    /// for serde round-tripping with providers that add new tool types.
    pub r#type: String,
    /// Schema of the function the model can call.
    pub function: FunctionDef,
}

/// Describes a callable function, including its JSON-schema parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    /// Name the model uses to reference this function.
    pub name: String,
    /// Human-readable description that helps the model decide when to call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema object describing the function parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A single function invocation produced by the model; in streaming deltas
/// `id`/`function` may be absent and chunks merge by `index`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Position inside the `tool_calls` array (streaming deltas only); `i32`
    /// matching the OpenAI wire format, clamped from `usize` by the encoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i32>,
    /// Stable identifier for this tool call (absent in deltas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Always `"function"` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Function name and arguments (absent in the very first delta).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCall>,
}

/// Name + arguments of a function call; both `Option` to accommodate
/// streaming deltas that merge by `index`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The function name chosen by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// JSON-encoded argument object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// A single message in a conversation. Assistant messages may carry
/// `tool_calls` with empty `content`; unknown fields are ignored (no
/// `deny_unknown_fields`) because streaming deltas vary by provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `"system"`, `"user"`, `"assistant"`, or `"tool"`; empty when absent
    /// (streaming deltas omit it). Known-role validation happens at decode.
    #[serde(default)]
    pub role: String,
    /// The content body: a plain string or an array of structured content
    /// parts (e.g. text/`image_url`); `serde_json::Value` accepts both forms.
    /// Defaults to `""` when absent (streaming deltas often omit it).
    #[serde(default = "default_content")]
    pub content: serde_json::Value,
    /// Chain-of-thought text from reasoning models; the `reasoning` alias
    /// covers providers (e.g. DeepSeek) that use that field name.
    #[serde(alias = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool calls emitted by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Sender name (used when `role` is `"tool"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Links this tool-result message to a specific [`ToolCall::id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Anthropic-style cache hint preserved across protocol translation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Model refusal text (OpenAI-specific: present on assistant messages when
    /// the model refuses to answer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Tool-result error flag (only meaningful when `role == "tool"`).
    /// Some OpenAI-compatible providers use `status` instead of `is_error`.
    #[serde(default, alias = "status", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ChatMessage {
    /// Extract the text content as a `String`: `Value::String` returned
    /// directly, array parts concatenate their `"type":"text"` `"text"`
    /// fields, anything else yields `""` (with a warning).
    pub fn content_text(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => {
                let mut out = String::new();
                for part in parts {
                    if let serde_json::Value::Object(map) = part
                        && map.get("type").and_then(|v| v.as_str()) == Some("text")
                        && let Some(text) = map.get("text").and_then(|v| v.as_str())
                    {
                        out.push_str(text);
                    }
                }
                out
            }
            serde_json::Value::Null => String::new(),
            other => {
                tracing::warn!(
                    ?other,
                    "ChatMessage::content_text: unexpected content type, returning empty string"
                );
                String::new()
            }
        }
    }

    /// Returns `true` when `content` is `Null`, an empty string, or an
    /// empty array.
    pub fn content_is_empty(&self) -> bool {
        match &self.content {
            serde_json::Value::String(s) => s.is_empty(),
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Null => true,
            _ => false,
        }
    }
}

fn default_content() -> serde_json::Value {
    serde_json::Value::String(String::new())
}

/// Request body for `POST /v1/chat/completions`, deserialized from inbound
/// client requests and constructed internally for outbound provider calls;
/// unknown client fields are captured in `extra` rather than dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// Model identifier (e.g. `"gpt-4o"`, `"glm-5.1"`).
    pub model: String,
    /// Conversation turns sent to the model.
    pub messages: Vec<ChatMessage>,
    /// Request a streaming response when `Some(true)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Sampling temperature in `[0, 2]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Upper bound on tokens generated in the response. OpenAI renamed
    /// `max_tokens` to `max_completion_tokens`; both are accepted (serde
    /// alias) and the canonical field follows the newer convention.
    #[serde(
        rename = "max_completion_tokens",
        alias = "max_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_tokens: Option<i32>,
    /// Reasoning effort level (e.g. `"low"`, `"medium"`, `"high"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Extended thinking configuration (provider-specific JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Tools the model is allowed to call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// Controls which (if any) tool the model must call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// Stop sequences (string or array of strings), kept as raw
    /// `serde_json::Value` for passthrough; `openai_chat::decode_request`
    /// normalises it into `SamplingOptions.stop`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<serde_json::Value>,
    /// Extra streaming metadata flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// End-user identifier for abuse monitoring.
    ///
    /// Maps to `RequestMetadata.user_id` during decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Catch-all for unrecognized client fields, preserved in
    /// `RequestMetadata.raw` during decode. `#[serde(flatten)]` precludes
    /// `deny_unknown_fields`, so typo'd fields land here silently.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Token usage statistics returned by the API; unknown provider fields are
/// ignored, and `Default` lets an absent or `null` `usage` on a 2xx decode
/// as zero counts instead of failing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UsageInfo {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: i32,
    /// Tokens produced by the completion.
    pub completion_tokens: i32,
    /// `prompt_tokens + completion_tokens`; recomputed via `saturating_add`
    /// during encoding, so the proxy's value wins over the provider's.
    pub total_tokens: i32,
    /// Prompt tokens served from a cache (Anthropic / provider-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<i32>,
    /// Prompt tokens that missed the cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<i32>,
}

fn deserialize_default_on_null<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    use serde::Deserialize as _;
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A single completion alternative inside a response or chunk; `message`
/// for non-streaming, `delta` for streaming chunks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    /// Position of this choice in the array.
    pub index: i32,
    /// The full assistant message (non-streaming responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<ChatMessage>,
    /// Why the model stopped (e.g. `"stop"`, `"tool_calls"`, `"length"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Partial message delta (streaming chunks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<ChatMessage>,
}

/// Response body for a non-streaming Chat Completions call. Unknown fields
/// are ignored rather than failing decode into a 502 (providers add fields
/// like `service_tier`); request types retain strict validation instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    /// Unique completion identifier.
    pub id: String,
    /// Object type, always `"chat.completion"`.
    pub object: String,
    /// Unix timestamp of creation.
    pub created: i64,
    /// Model that generated the response.
    pub model: String,
    /// Ordered list of completion alternatives.
    pub choices: Vec<Choice>,
    /// Token usage for this request. Some providers omit `usage` or send
    /// `null` on 2xx; `#[serde(default)]` + `deserialize_default_on_null`
    /// map both to zero token counts instead of a decode failure.
    #[serde(default, deserialize_with = "deserialize_default_on_null")]
    pub usage: UsageInfo,
    /// Catch-all for provider-specific response fields; `#[serde(flatten)]`
    /// precludes `deny_unknown_fields`, so unknown fields are captured.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A single Server-Sent Events chunk for a streaming completion; unknown
/// fields are ignored because upstream providers add fields at any time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    /// Unique completion identifier (stable across all chunks).
    pub id: String,
    /// Object type, always `"chat.completion.chunk"`.
    pub object: String,
    /// Unix timestamp of creation.
    pub created: i64,
    /// Model that generated the chunk.
    pub model: String,
    /// Ordered list of delta choices.
    pub choices: Vec<Choice>,
    /// Token usage (present only on the final chunk when `include_usage` was requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
}

/// Top-level error envelope returned by OpenAI-compatible APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Nested error details.
    pub error: ErrorDetails,
}

/// Detailed error information inside an [`ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorDetails {
    /// Machine-readable error category (e.g. `"invalid_request_error"`).
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Human-readable error description.
    pub message: String,
    /// Optional error code (e.g. `"invalid_api_key"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_round_trip() {
        let cc = CacheControl {
            r#type: "ephemeral".into(),
        };
        let json = serde_json::to_string(&cc).unwrap();
        assert_eq!(json, r#"{"type":"ephemeral"}"#);
        let back: CacheControl = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cc);
    }

    #[test]
    fn error_response_round_trip() {
        let err = ErrorResponse {
            error: ErrorDetails {
                r#type: Some("invalid_request_error".into()),
                message: "model is required".into(),
                code: Some("invalid_api_key".into()),
            },
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, err);
    }

    #[test]
    fn tool_def_round_trip() {
        let tool = ToolDef {
            r#type: "function".into(),
            function: FunctionDef {
                name: "get_weather".into(),
                description: Some("Get the weather".into()),
                parameters: Some(serde_json::json!({"type": "object"})),
            },
        };
        let json = serde_json::to_string(&tool).unwrap();
        let back: ToolDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tool);
    }

    #[test]
    fn chat_message_reasoning_alias() {
        let json = r#"{"role":"assistant","reasoning":"thinking..."}"#;
        let msg: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.reasoning_content.as_deref(), Some("thinking..."));
    }

    #[test]
    fn usage_info_round_trip() {
        let usage = UsageInfo {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_cache_hit_tokens: Some(10),
            prompt_cache_miss_tokens: None,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: UsageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn chat_completion_request_accepts_unknown_fields() {
        let json = r#"{"model":"gpt-4o","messages":[],"future_field":"value"}"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.extra.get("future_field").unwrap().as_str(),
            Some("value")
        );
    }

    #[test]
    fn choice_round_trip() {
        let choice = Choice {
            index: 0,
            message: Some(ChatMessage {
                role: "assistant".into(),
                content: serde_json::Value::String("hello".into()),
                reasoning_content: None,
                tool_calls: vec![],
                name: None,
                tool_call_id: None,
                cache_control: None,
                refusal: None,
                is_error: None,
            }),
            finish_reason: Some("stop".into()),
            delta: None,
        };
        let json = serde_json::to_string(&choice).unwrap();
        let back: Choice = serde_json::from_str(&json).unwrap();
        assert_eq!(back, choice);
    }
}
