//! Prefix-stability tests (plan 06 step 6, audit conflict 5): on a
//! same-format route, no enabled feature changes the prompt prefix — the
//! `tools`, `system` and `messages` content up to and including the last
//! cache breakpoint — unless the operator opted into prompt-cache hygiene.
//!
//! These are the tests the plan's done-when names: "no enabled feature
//! changes bytes before the last breakpoint unless opted in".

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Arc;

use llm_proxy_adapters::client_wire::{ClientWireFormat, ClientWireRequest};
use llm_proxy_adapters::protocols;
use llm_proxy_adapters::{AnthropicAdapter, OpenAiChatAdapter, ProviderAdapterTarget};
use llm_proxy_core::{AuthStyle, PromptCacheConfig};
use llm_proxy_protocol::core::CoreRequest;
use secrecy::SecretString;
use serde_json::{Value, json};

fn target(prompt_cache: Option<PromptCacheConfig>) -> ProviderAdapterTarget {
    ProviderAdapterTarget {
        provider_name: "anthropic".to_owned(),
        adapter_name: "messages".to_owned(),
        protocol: llm_proxy_adapters::ProtocolName::new(protocols::ANTHROPIC_MESSAGES),
        endpoint: "https://upstream.example/v1/messages".to_owned(),
        auth_style: AuthStyle::XApiKey,
        api_key: SecretString::from("upstream-key"),
        requested_model: "claude-sonnet-4-6".to_owned(),
        upstream_model: "claude-sonnet-4-20250514".to_owned(),
        headers: Arc::new(HashMap::new()),
        lossy: false,
        forward_inbound_headers: Arc::new(Vec::new()),
        prompt_cache: prompt_cache.map(Arc::new),
    }
}

/// A client body with cache markers on the stable prefix: the system block
/// and every message except the newest carry `cache_control`.
fn marked_client_body() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 1024,
        "metadata": {"user_id": "turn-7-correlation"},
        "system": [
            {"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "First turn.", "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "First reply."}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "Newest turn — changes every request."}
            ]}
        ]
    })
}

fn core_request() -> CoreRequest {
    // The real client decode of the same body, as the server produces it.
    let typed: llm_proxy_protocol::anthropic::MessageRequest =
        serde_json::from_value(marked_client_body()).expect("wire request parses");
    llm_proxy_protocol::client::anthropic::decode_request(typed).expect("core request decodes")
}

fn wire(body: &Value) -> ClientWireRequest {
    ClientWireRequest::new(ClientWireFormat::AnthropicMessages, body.clone())
}

fn encode(body: &Value, target: &ProviderAdapterTarget) -> Value {
    let request = AnthropicAdapter
        .encode_request(&core_request(), target, &wire(body))
        .expect("encode");
    serde_json::from_slice(&request.body).expect("encoded body is JSON")
}

/// The prefix the provider's prompt cache keys on: tools, system, and every
/// message up to and including the block carrying the last cache marker.
fn prefix_of(body: &Value) -> Value {
    let mut prefix = json!({});
    if let Some(map) = prefix.as_object_mut() {
        if let Some(tools) = body.get("tools") {
            map.insert("tools".to_owned(), tools.clone());
        }
        if let Some(system) = body.get("system") {
            map.insert("system".to_owned(), system.clone());
        }
        let messages = body.get("messages").and_then(Value::as_array);
        let mut cut = messages.map_or(0, Vec::len);
        if let Some(messages) = messages {
            for (message_index, message) in messages.iter().enumerate() {
                let blocks = message.get("content").and_then(Value::as_array);
                if blocks.is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block.get("cache_control").is_some_and(|v| !v.is_null()))
                }) {
                    cut = message_index + 1;
                }
            }
        }
        if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            map.insert(
                "messages".to_owned(),
                Value::Array(messages.iter().take(cut).cloned().collect()),
            );
        }
    }
    prefix
}

#[test]
fn default_features_keep_the_marked_prefix_byte_identical() {
    // Every phase-1/phase-2 feature is on: model aliasing (the target maps
    // to another upstream model). Still, the prompt prefix before the last
    // cache breakpoint must be exactly the client's.
    let client = marked_client_body();
    let encoded = encode(&client, &target(None));

    assert_eq!(
        prefix_of(&client),
        prefix_of(&encoded),
        "the prefix before the last breakpoint changed: {encoded}"
    );
    // The route edit (phase 1) does apply — to the routing field only.
    assert_eq!(encoded["model"], json!("claude-sonnet-4-20250514"));
}

#[test]
fn hygiene_off_by_default_leaves_volatile_bytes_in_place() {
    let client = marked_client_body();
    let encoded = encode(&client, &target(None));
    assert_eq!(
        encoded["metadata"]["user_id"],
        json!("turn-7-correlation"),
        "without the opt-in nothing is stripped"
    );
    assert!(
        encoded["messages"][2]["content"][0]["cache_control"].is_null(),
        "without the opt-in no markers are injected"
    );
}

#[test]
fn opted_in_hygiene_strips_volatile_bytes_only() {
    let client = marked_client_body();
    let config = PromptCacheConfig {
        strip_volatile: vec!["/metadata/user_id".to_owned()],
        inject_markers: false,
    };
    let encoded = encode(&client, &target(Some(config)));

    // The volatile pointer is gone; the prefix is otherwise untouched.
    assert!(
        encoded["metadata"].is_object() && encoded["metadata"].get("user_id").is_none(),
        "the configured volatile pointer must be stripped: {}",
        encoded["metadata"]
    );
    assert_eq!(
        prefix_of(&client),
        prefix_of(&encoded),
        "stripping volatile bytes must not disturb the prefix"
    );
    // Marker injection was not opted into.
    assert!(encoded["messages"][2]["content"][0]["cache_control"].is_null());
}

#[test]
fn opted_in_marker_injection_marks_only_unmarked_stable_blocks() {
    let client = marked_client_body();
    let config = PromptCacheConfig {
        strip_volatile: Vec::new(),
        inject_markers: true,
    };
    let encoded = encode(&client, &target(Some(config)));

    // The client's own markers are kept, verbatim and in place.
    assert_eq!(
        encoded["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        encoded["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    // The newest message stays unmarked.
    assert!(encoded["messages"][2]["content"][0]["cache_control"].is_null());
    // Two client markers spent two of four breakpoints: the remaining
    // budget marks the assistant message's last block (the newest-but-one
    // turn), not the newest.
    assert!(
        encoded["messages"][1]["content"][0]["cache_control"].is_object(),
        "the leftover breakpoint lands on the stable prefix: {encoded}"
    );
}

#[test]
fn chat_adapter_strips_volatile_bytes_but_injects_no_markers() {
    // The Chat Completions wire format has no cache markers; the hygiene
    // opt-in still strips volatile bytes at the provider edge.
    let client = json!({
        "model": "gpt-4o",
        "user": "correlation-token-9",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let core: CoreRequest = llm_proxy_protocol::client::openai_chat::decode_request(
        serde_json::from_value::<llm_proxy_protocol::openai::ChatCompletionRequest>(client.clone())
            .expect("wire request parses"),
    )
    .expect("core decodes");
    let mut chat_target = target(Some(PromptCacheConfig {
        strip_volatile: vec!["/user".to_owned()],
        inject_markers: true,
    }));
    chat_target.protocol =
        llm_proxy_adapters::ProtocolName::new(protocols::OPENAI_CHAT_COMPLETIONS);
    chat_target.upstream_model = "gpt-4o-2024-11-20".to_owned();

    let request = OpenAiChatAdapter
        .encode_request(
            &core,
            &chat_target,
            &ClientWireRequest::new(ClientWireFormat::OpenAiChatCompletions, client),
        )
        .expect("encode");
    let encoded: Value = serde_json::from_slice(&request.body).expect("JSON");
    assert!(encoded.get("user").is_none(), "volatile field stripped");
    assert_eq!(
        encoded["messages"],
        json!([{"role": "user", "content": "hello"}]),
        "no marker fields are injected on chat"
    );
}
