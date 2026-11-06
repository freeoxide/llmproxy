//! Anthropic API types for the Messages API.
//!
//! Reference: <https://docs.anthropic.com/en/api/messages>

use serde::{Deserialize, Serialize, Serializer};

/// A request to the Anthropic Messages API. Unknown fields are tolerated (no
/// `deny_unknown_fields`, they land in `extra`) so same-format passthrough can
/// forward them and the cross-format policy can name them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageRequest {
    /// The model identifier (e.g. "claude-sonnet-4-20250514").
    pub model: String,
    /// Maximum number of tokens to generate.
    pub max_tokens: i32,
    /// Optional system prompt. Can be a plain string or an array of [`SystemContentBlock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    /// The conversation messages.
    pub messages: Vec<Message>,
    /// Whether to stream the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Tool definitions available to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-K sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    /// Custom stop sequences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Optional metadata attached to the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Extended thinking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Controls which (if any) tool the model must call. Accepts
    /// `{"type": "auto"}`, `{"type": "any"}`, or `{"type": "tool", "name": "..."}`
    /// as defined by the Anthropic Messages API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// Unrecognized request fields; preserved so same-format passthrough can
    /// forward them and the cross-format policy can reject them by name.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl MessageRequest {
    /// Extracts the system prompt text from the `system` field, which may be
    /// a plain string or an array of [`SystemContentBlock`]s; array text blocks
    /// are joined with `\n`, and anything unparseable falls back to raw JSON.
    #[must_use]
    pub fn system_text(&self) -> String {
        let Some(value) = &self.system else {
            return String::new();
        };
        if let Some(s) = value.as_str() {
            return s.to_owned();
        }
        if let Some(arr) = value.as_array() {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|item| serde_json::from_value::<SystemContentBlock>(item.clone()).ok())
                .filter(|block| block.r#type == "text")
                .filter_map(|block| block.text)
                .collect();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
        value.to_string()
    }

    /// Validates that required fields are present; returns
    /// `ProtocolError::InvalidRequest` describing the first problem otherwise.
    pub fn validate(&self) -> Result<(), crate::client::ProtocolError> {
        if self.model.is_empty() {
            return Err(crate::client::ProtocolError::InvalidRequest(
                "model is required".to_owned(),
            ));
        }
        if self.messages.is_empty() {
            return Err(crate::client::ProtocolError::InvalidRequest(
                "messages is required".to_owned(),
            ));
        }
        if self.max_tokens <= 0 {
            return Err(crate::client::ProtocolError::InvalidRequest(format!(
                "max_tokens must be positive, got {}",
                self.max_tokens
            )));
        }
        Ok(())
    }
}

/// A content block inside the `system` array. Unknown fields are tolerated
/// (no `deny_unknown_fields`) so future Anthropic system-block options decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemContentBlock {
    /// Block type, typically `"text"`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// The text content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Cache control directive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Cache control directives for prompt caching. Unknown fields are tolerated
/// so a new cache option can never invalidate the block that carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    /// Cache control type, typically `"ephemeral"`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Cache lifetime such as `"5m"` or `"1h"`; the provider default applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// Optional metadata attached to a [`MessageRequest`]; unknown fields are
/// preserved in `extra` (via `#[serde(flatten)]`, so `deny_unknown_fields`
/// is not set) and forwarded to `RequestMetadata.raw`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// An external identifier for the end-user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Catch-all for unrecognized metadata fields from clients.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A single message in the conversation; `content` is either a plain string
/// or an array of [`ContentBlock`]s. Unknown fields are tolerated (no
/// `deny_unknown_fields`) so future Anthropic fields decode cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Message {
    /// The role of the message author (`"user"` or `"assistant"`).
    pub role: String,
    /// Message content – a string or array of content blocks.
    pub content: serde_json::Value,
}

impl Message {
    /// Parses the message `content` into a list of [`ContentBlock`]s; accepts
    /// both `"content": "text"` and `"content": [block, ...]` wire shapes.
    /// Errors on any other shape or a malformed block, so content is never
    /// dropped silently.
    #[expect(deprecated, reason = "constructs ContentBlock with output: None")]
    pub fn content_blocks(&self) -> Result<Vec<ContentBlock>, serde_json::Error> {
        if self.content.is_null() {
            return Ok(Vec::new());
        }
        if let Some(s) = self.content.as_str() {
            return Ok(vec![ContentBlock {
                r#type: "text".to_owned(),
                text: Some(s.to_owned()),
                id: None,
                tool_use_id: None,
                name: None,
                input: None,
                output: None,
                content: None,
                is_error: None,
                thinking: None,
                signature: None,
                source: None,
                cache_control: None,
                data: None,
            }]);
        }
        if let Some(arr) = self.content.as_array() {
            return arr
                .iter()
                .map(|item| serde_json::from_value::<ContentBlock>(item.clone()))
                .collect();
        }
        Err(serde::de::Error::custom(
            "message content must be a string or an array of blocks",
        ))
    }
}

/// A block of content within a message; `r#type` selects the populated
/// fields (`"text"`, `"tool_use"`, `"tool_result"`, `"thinking"`,
/// `"redacted_thinking"`, `"image"`, `"document"`). Unknown types must be
/// tolerated.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ContentBlock {
    /// Block type discriminator.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Text content (for `"text"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool call identifier (for `"tool_use"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The tool call ID this result refers to (for `"tool_result"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Tool name (for `"tool_use"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool input (for `"tool_use"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    /// Deprecated: use `content` instead.
    #[deprecated(since = "0.1.0", note = "use `content` field instead")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Inner content for `"tool_result"` blocks (string or array).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    /// Whether the tool result is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Thinking text (for `"thinking"` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Signature for the thinking block (extended thinking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Media source of `"image"` and `"document"` blocks, kept as opaque JSON
    /// so every documented shape (`base64`, `url`, `file`, ...) decodes;
    /// adapters interpret only the shapes they understand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    /// Cache control directive (for `"text"` blocks, but potentially others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Opaque data for `"redacted_thinking"` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[expect(
    deprecated,
    reason = "constructors set `output: None` which is the deprecated field"
)]
impl ContentBlock {
    /// Create a text content block with the given string.
    #[must_use]
    pub fn new_text(text: String) -> Self {
        ContentBlock {
            r#type: "text".to_owned(),
            text: Some(text),
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        }
    }

    /// Create a tool_use content block.
    #[must_use]
    pub fn new_tool_use(id: String, name: String, input: serde_json::Value) -> Self {
        ContentBlock {
            r#type: "tool_use".to_owned(),
            text: None,
            id: Some(id),
            tool_use_id: None,
            name: Some(name),
            input: Some(input),
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        }
    }

    /// Create a thinking content block.
    #[must_use]
    pub fn new_thinking(thinking: String) -> Self {
        ContentBlock {
            r#type: "thinking".to_owned(),
            text: None,
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: Some(thinking),
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        }
    }

    /// Create a redacted_thinking content block.
    #[must_use]
    pub fn new_redacted_thinking(data: String) -> Self {
        ContentBlock {
            r#type: "redacted_thinking".to_owned(),
            text: None,
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: Some(data),
        }
    }

    /// Create an image content block.
    #[must_use]
    pub fn new_image(source: serde_json::Value) -> Self {
        ContentBlock {
            r#type: "image".to_owned(),
            text: None,
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: Some(source),
            cache_control: None,
            data: None,
        }
    }

    /// Create a tool_result content block.
    #[must_use]
    pub fn new_tool_result(
        tool_use_id: String,
        content: Option<serde_json::Value>,
        is_error: Option<bool>,
    ) -> Self {
        ContentBlock {
            r#type: "tool_result".to_owned(),
            text: None,
            id: None,
            tool_use_id: Some(tool_use_id),
            name: None,
            input: None,
            output: None,
            content,
            is_error,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        }
    }

    /// Returns the tool ID for this block: `tool_use_id` for `"tool_result"`,
    /// `id` for every other type.
    #[must_use]
    pub fn get_tool_id(&self) -> &str {
        if self.r#type == "tool_result" {
            self.tool_use_id.as_deref().unwrap_or("")
        } else {
            self.id.as_deref().unwrap_or("")
        }
    }

    /// Extracts text from a `"tool_result"` block's `content` field (string
    /// or array of text blocks), falling back to the deprecated `output`.
    #[must_use]
    pub fn text_content(&self) -> String {
        if let Some(ref val) = self.content {
            if let Some(s) = val.as_str() {
                return s.to_owned();
            }
            if let Some(arr) = val.as_array() {
                let text: String = arr
                    .iter()
                    .filter_map(serde_json::Value::as_object)
                    .filter(|obj| obj.get("type").and_then(|v| v.as_str()) == Some("text"))
                    .filter_map(|obj| obj.get("text").and_then(|v| v.as_str()))
                    .collect();
                if !text.is_empty() {
                    return text;
                }
            }
        }
        #[expect(
            deprecated,
            reason = "reads the deprecated `output` field as a text fallback"
        )]
        if let Some(ref val) = self.output {
            if let Some(s) = val.as_str() {
                return s.to_owned();
            }
            return val.to_string();
        }
        String::new()
    }
}

fn serialize_unknown_block<S>(block: &ContentBlock, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[derive(Serialize)]
    struct AllFields {
        #[serde(rename = "type")]
        r#type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    }

    #[expect(
        deprecated,
        reason = "AllFields debug aid mirrors the deprecated `output` field"
    )]
    let all = AllFields {
        r#type: block.r#type.clone(),
        text: block.text.clone(),
        id: block.id.clone(),
        tool_use_id: block.tool_use_id.clone(),
        name: block.name.clone(),
        input: block.input.clone(),
        output: block.output.clone(),
        content: block.content.clone(),
        is_error: block.is_error,
        thinking: block.thinking.clone(),
        signature: block.signature.clone(),
        source: block.source.clone(),
        cache_control: block.cache_control.clone(),
        data: block.data.clone(),
    };
    all.serialize(serializer)
}

/// Custom [`Serialize`] for [`ContentBlock`] that emits only the fields
/// relevant to each block type, matching the strict Anthropic API schema.
impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;

        match self.r#type.as_str() {
            "text" => {
                let has_cache = self.cache_control.is_some();
                let len = if has_cache { 3 } else { 2 };
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry("text", self.text.as_deref().unwrap_or(""))?;
                if let Some(ref cc) = self.cache_control {
                    map.serialize_entry("cache_control", cc)?;
                }
                map.end()
            }
            "tool_use" => {
                let empty_object = serde_json::Value::Object(serde_json::Map::new());
                let input = self.input.as_ref().unwrap_or(&empty_object);
                let has_sig = self.signature.is_some();
                let len = if has_sig { 5 } else { 4 };
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry("id", self.id.as_deref().unwrap_or(""))?;
                map.serialize_entry("name", self.name.as_deref().unwrap_or(""))?;
                map.serialize_entry("input", input)?;
                if let Some(ref sig) = self.signature {
                    map.serialize_entry("signature", sig)?;
                }
                map.end()
            }
            "tool_result" => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry("tool_use_id", self.tool_use_id.as_deref().unwrap_or(""))?;
                if let Some(ref content) = self.content {
                    map.serialize_entry("content", content)?;
                }
                if let Some(is_error) = self.is_error {
                    map.serialize_entry("is_error", &is_error)?;
                }
                if let Some(ref cc) = self.cache_control {
                    map.serialize_entry("cache_control", cc)?;
                }
                map.end()
            }
            "thinking" => {
                let has_sig = self.signature.is_some();
                let len = if has_sig { 3 } else { 2 };
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry("thinking", self.thinking.as_deref().unwrap_or(""))?;
                if let Some(ref sig) = self.signature {
                    map.serialize_entry("signature", sig)?;
                }
                map.end()
            }
            "redacted_thinking" => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry("data", self.data.as_deref().unwrap_or(""))?;
                map.end()
            }
            "image" => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", &self.r#type)?;
                map.serialize_entry(
                    "source",
                    self.source.as_ref().unwrap_or(&serde_json::Value::Null),
                )?;
                map.end()
            }
            _ => serialize_unknown_block(self, serializer),
        }
    }
}

/// A tool definition for function calling. Unknown fields (server-tool
/// discriminators, cache markers, future options) are tolerated so they decode
/// on the request path; same-format passthrough forwards the raw tool JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// The tool name.
    pub name: String,
    /// Human-readable description of what the tool does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters; optional because
    /// server tools (e.g. `web_search`) carry no schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    /// Unrecognized tool fields (server-tool `type`, `cache_control`, ...),
    /// preserved for passthrough and named rejection cross-format.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The result of executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// The tool call ID this result corresponds to.
    pub tool_use_id: String,
    /// The result content.
    pub content: String,
    /// Whether the tool execution resulted in an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A response from the Anthropic Messages API, built by the encode path
/// and deserialized from upstream `message_start` SSE snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageResponse {
    /// Unique message identifier.
    pub id: String,
    /// Response type, always `"message"`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Conversation role, always `"assistant"`.
    pub role: String,
    /// The content blocks generated by the model.
    pub content: Vec<ContentBlock>,
    /// The model that produced this response.
    pub model: String,
    /// The reason generation stopped (e.g. `"end_turn"`, `"max_tokens"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// The stop sequence that caused generation to stop, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Token usage statistics.
    pub usage: Usage,
}

/// Token usage statistics returned with every response; counts are `i32`,
/// matching the Anthropic JSON integer wire format. `message_delta` usage
/// objects carry only `output_tokens` (cumulative), so every field defaults
/// rather than failing the whole event's parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Number of tokens in the input prompt.
    #[serde(default)]
    pub input_tokens: i32,
    /// Number of tokens in the generated output.
    #[serde(default)]
    pub output_tokens: i32,
    /// Tokens used to create a new cache entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i32>,
    /// Tokens read from an existing cache entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i32>,
    /// Breakdown of cache creation tokens by TTL window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
}

/// Per-TTL breakdown of cache-creation tokens on an Anthropic usage object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CacheCreation {
    /// Tokens written to a 5-minute ephemeral cache entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<i32>,
    /// Tokens written to a 1-hour ephemeral cache entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<i32>,
}

impl CacheCreation {
    /// Total tokens across both TTL windows, saturating on overflow.
    #[must_use]
    pub fn total_input_tokens(&self) -> i32 {
        self.ephemeral_5m_input_tokens
            .unwrap_or(0)
            .saturating_add(self.ephemeral_1h_input_tokens.unwrap_or(0))
    }
}

/// A streaming delta event for a content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlockDelta {
    /// Event type, always `"content_block_delta"`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Index of the content block this delta applies to.
    pub index: usize,
    /// The partial update.
    pub delta: Delta,
}

/// A partial update in a streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Delta {
    /// Delta type (e.g. `"text_delta"`, `"thinking_delta"`, `"input_json_delta"`).
    /// `Option` with `#[serde(default)]` for forward compatibility; `None`
    /// indicates a malformed or unknown delta.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Partial text content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Partial thinking content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Partial JSON for tool input streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_json: Option<String>,
    /// Stop reason (on the final delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Stop sequence (on the final delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// A Server-Sent Event from the Anthropic streaming API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageEvent {
    /// Event type (e.g. `"message_start"`, `"content_block_start"`,
    /// `"content_block_delta"`, `"message_stop"`).
    #[serde(rename = "type")]
    pub r#type: String,
    /// The full message (present on `"message_start"` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageResponse>,
    /// Content block index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// A content block (present on `"content_block_start"` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_block: Option<ContentBlock>,
    /// A streaming delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<Delta>,
    /// Token usage (present on `"message_delta"` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// An error from the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// An error returned by the Anthropic API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    /// Error type (e.g. `"invalid_request_error"`).
    #[serde(rename = "type")]
    pub r#type: String,
    /// Human-readable error description.
    pub message: String,
}

#[cfg(test)]
#[expect(
    deprecated,
    reason = "tests construct ContentBlock with `output` field"
)]
mod tests {
    use super::*;

    #[test]
    fn system_text_none_when_absent() {
        let req = MessageRequest {
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 1024,
            system: None,
            messages: vec![Message {
                role: "user".into(),
                content: serde_json::Value::String("hi".into()),
            }],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        assert_eq!(req.system_text(), "");
    }

    #[test]
    fn system_text_plain_string() {
        let req = MessageRequest {
            model: "test".into(),
            max_tokens: 100,
            system: Some(serde_json::Value::String("You are helpful".into())),
            messages: vec![Message {
                role: "user".into(),
                content: serde_json::Value::String("hi".into()),
            }],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        assert_eq!(req.system_text(), "You are helpful");
    }

    #[test]
    fn system_text_array_of_blocks() {
        let raw = serde_json::json!([
            { "type": "text", "text": "You are helpful. " },
            { "type": "text", "text": "Be concise." }
        ]);
        let req = MessageRequest {
            model: "test".into(),
            max_tokens: 100,
            system: Some(raw),
            messages: vec![Message {
                role: "user".into(),
                content: serde_json::Value::String("hi".into()),
            }],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        assert_eq!(req.system_text(), "You are helpful. \nBe concise.");
    }

    #[test]
    fn content_blocks_from_string() {
        let msg = Message {
            role: "user".into(),
            content: serde_json::Value::String("hello world".into()),
        };
        let blocks = msg.content_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].r#type, "text");
        assert_eq!(blocks[0].text.as_deref(), Some("hello world"));
    }

    #[test]
    fn content_blocks_from_array() {
        let raw = serde_json::json!([
            { "type": "text", "text": "hello" },
            { "type": "tool_use", "id": "tu_1", "name": "get_weather", "input": {"city":"SF"} }
        ]);
        let msg = Message {
            role: "assistant".into(),
            content: raw,
        };
        let blocks = msg.content_blocks().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].r#type, "text");
        assert_eq!(blocks[0].text.as_deref(), Some("hello"));
        assert_eq!(blocks[1].r#type, "tool_use");
        assert_eq!(blocks[1].id.as_deref(), Some("tu_1"));
        assert_eq!(blocks[1].name.as_deref(), Some("get_weather"));
    }

    #[test]
    fn content_blocks_null_returns_empty() {
        let msg = Message {
            role: "user".into(),
            content: serde_json::Value::Null,
        };
        assert!(msg.content_blocks().unwrap().is_empty());
    }

    #[test]
    fn content_blocks_rejects_a_malformed_block_instead_of_dropping_it() {
        let msg = Message {
            role: "user".into(),
            content: serde_json::json!([
                { "type": "text", "text": "keep me" },
                { "type": "text", "text": 42 }
            ]),
        };
        msg.content_blocks().unwrap_err();
    }

    #[test]
    fn content_blocks_rejects_content_that_is_not_text_or_blocks() {
        let msg = Message {
            role: "user".into(),
            content: serde_json::json!(42),
        };
        msg.content_blocks().unwrap_err();
    }

    #[test]
    fn content_blocks_accepts_a_cache_option_it_does_not_know() {
        let msg = Message {
            role: "user".into(),
            content: serde_json::json!([{
                "type": "text",
                "text": "keep me",
                "cache_control": { "type": "ephemeral", "ttl": "1h", "future_option": true }
            }]),
        };
        let blocks = msg.content_blocks().unwrap();
        assert_eq!(blocks[0].text.as_deref(), Some("keep me"));
        let cache = blocks[0].cache_control.as_ref().unwrap();
        assert_eq!(cache.ttl.as_deref(), Some("1h"));
    }

    #[test]
    fn validate_ok() {
        let req = MessageRequest {
            model: "claude-3".into(),
            max_tokens: 100,
            system: None,
            messages: vec![Message {
                role: "user".into(),
                content: serde_json::Value::String("hi".into()),
            }],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        req.validate().expect("request should validate");
    }

    #[test]
    fn validate_missing_model() {
        let req = MessageRequest {
            model: String::new(),
            max_tokens: 100,
            system: None,
            messages: vec![Message {
                role: "user".into(),
                content: serde_json::Value::String("hi".into()),
            }],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        let err = req.validate().unwrap_err();
        assert!(
            matches!(err, crate::client::ProtocolError::InvalidRequest(ref msg) if msg == "model is required")
        );
    }

    #[test]
    fn validate_missing_messages() {
        let req = MessageRequest {
            model: "claude-3".into(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: None,
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        let err = req.validate().unwrap_err();
        assert!(
            matches!(err, crate::client::ProtocolError::InvalidRequest(ref msg) if msg == "messages is required")
        );
    }

    #[test]
    fn serialize_text_block() {
        let block = ContentBlock {
            r#type: "text".into(),
            text: Some("hello".into()),
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn serialize_text_block_with_cache_control() {
        let block = ContentBlock {
            r#type: "text".into(),
            text: Some("hello".into()),
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: Some(CacheControl {
                r#type: "ephemeral".into(),
                ttl: None,
            }),
            data: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["cache_control"]["type"], "ephemeral");
        assert_eq!(json.as_object().unwrap().len(), 3);
    }

    #[test]
    fn serialize_tool_use_block() {
        let block = ContentBlock {
            r#type: "tool_use".into(),
            text: None,
            id: Some("tu_1".into()),
            tool_use_id: None,
            name: Some("get_weather".into()),
            input: Some(serde_json::json!({"city": "SF"})),
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tu_1");
        assert_eq!(json["name"], "get_weather");
        assert_eq!(json["input"]["city"], "SF");
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn serialize_tool_result_block() {
        let block = ContentBlock {
            r#type: "tool_result".into(),
            text: None,
            id: None,
            tool_use_id: Some("tu_1".into()),
            name: None,
            input: None,
            output: None,
            content: Some(serde_json::json!("72F and sunny")),
            is_error: Some(false),
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "tu_1");
        assert_eq!(json["content"], "72F and sunny");
        assert_eq!(json["is_error"], false);
    }

    #[test]
    fn serialize_thinking_block() {
        let block = ContentBlock {
            r#type: "thinking".into(),
            text: None,
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: Some("hmm...".into()),
            signature: Some("sig_abc".into()),
            source: None,
            cache_control: None,
            data: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["thinking"], "hmm...");
        assert_eq!(json["signature"], "sig_abc");
        assert_eq!(json.as_object().unwrap().len(), 3);
    }

    #[test]
    fn text_content_from_string_content() {
        let block = ContentBlock {
            r#type: "tool_result".into(),
            text: None,
            id: None,
            tool_use_id: Some("tu_1".into()),
            name: None,
            input: None,
            output: None,
            content: Some(serde_json::Value::String("result text".into())),
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        assert_eq!(block.text_content(), "result text");
    }

    #[test]
    fn text_content_from_array_content() {
        let raw = serde_json::json!([
            { "type": "text", "text": "part one" },
            { "type": "text", "text": "part two" }
        ]);
        let block = ContentBlock {
            r#type: "tool_result".into(),
            text: None,
            id: None,
            tool_use_id: Some("tu_1".into()),
            name: None,
            input: None,
            output: None,
            content: Some(raw),
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        assert_eq!(block.text_content(), "part onepart two");
    }

    #[test]
    fn text_content_fallback_to_output() {
        let block = ContentBlock {
            r#type: "tool_result".into(),
            text: None,
            id: None,
            tool_use_id: Some("tu_1".into()),
            name: None,
            input: None,
            output: Some(serde_json::Value::String("legacy output".into())),
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        assert_eq!(block.text_content(), "legacy output");
    }

    #[test]
    fn get_tool_id_tool_result() {
        let block = ContentBlock {
            r#type: "tool_result".into(),
            tool_use_id: Some("tu_42".into()),
            ..make_minimal_block()
        };
        assert_eq!(block.get_tool_id(), "tu_42");
    }

    #[test]
    fn get_tool_id_tool_use() {
        let block = ContentBlock {
            r#type: "tool_use".into(),
            id: Some("tu_99".into()),
            ..make_minimal_block()
        };
        assert_eq!(block.get_tool_id(), "tu_99");
    }

    fn make_minimal_block() -> ContentBlock {
        ContentBlock {
            r#type: String::new(),
            text: None,
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            output: None,
            content: None,
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        }
    }

    #[test]
    fn text_content_from_empty_string_content() {
        let block = ContentBlock {
            r#type: "tool_result".into(),
            text: None,
            id: None,
            tool_use_id: Some("tu_1".into()),
            name: None,
            input: None,
            output: None,
            content: Some(serde_json::Value::String(String::new())),
            is_error: None,
            thinking: None,
            signature: None,
            source: None,
            cache_control: None,
            data: None,
        };
        assert_eq!(block.text_content(), "");
    }
}
