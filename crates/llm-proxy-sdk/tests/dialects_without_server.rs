//! A dialect can be used from the SDK without the server crate (plan 08,
//! done-when #3): decode a client dialect with the `llm-proxy-protocol`
//! client codecs, run it through the engine against a loopback fake
//! upstream, and encode the answer back into the dialect — touching no
//! `llm-proxy-server` item. The suite also passes under
//! `--no-default-features`, where the server crate is not compiled at all.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use llm_proxy_sdk::prelude::*;
use serde_json::json;

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_fake_upstream() -> String {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&json!({
                        "id": "msg_sdk_fixture",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "pong"}],
                        "model": "claude-sonnet-4-5",
                        "stop_reason": "end_turn",
                        "stop_sequence": None::<String>,
                        "usage": {"input_tokens": 9, "output_tokens": 2}
                    }))
                    .expect("serialize"),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("fake upstream stopped: {error}");
            }
        });
        format!("http://{addr}")
    }

    fn app_config() -> AppConfig {
        AppConfig {
            server: core::ServerConfig {
                bind: "127.0.0.1:0".parse().expect("bind"),
                request_timeout: Duration::from_secs(30),
                shutdown_timeout: Duration::from_secs(5),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "dialect-sdk-test".to_owned(),
                rate_limit_rpm: 10_000,
                trust_forwarded_headers: false,
                cache: Default::default(),
                idempotency: Default::default(),
                metrics: Default::default(),
                log_format: Default::default(),
                admin_bind: None,
                admin_on_main_listener: false,
                endpoint_families: Default::default(),
                upstream_timeouts: Default::default(),
                bare_v1: Default::default(),
                preflight: Default::default(),
                semantic_cache: Default::default(),
            },
            auth: Default::default(),
            storage: None,
            replica: None,
            mcp: None,
        }
    }

    #[tokio::test]
    async fn the_responses_dialect_runs_through_the_engine_without_the_server() {
        let upstream = spawn_fake_upstream().await;
        let config = app_config();
        let provider = core::ProviderConfig {
            name: "sdk-provider".to_owned(),
            api_key: secrecy::SecretString::from("operator-key"),
            auth_style: core::AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "upstream".to_owned(),
                core::ProviderAdapterConfig {
                    protocol: "anthropic_messages".to_owned(),
                    endpoint: format!("{upstream}/v1/messages"),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
                },
            )]),
            routes: core::ProviderRoutesConfig {
                responses: Some("upstream".to_owned()),
                ..Default::default()
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
        };
        let engine = ProxyBuilder::new()
            .app_config(config)
            .provider(provider)
            .engine()
            .expect("engine builds");

        // Responses dialect in: decode with the protocol client codec only.
        let body = json!({
            "model": "gpt-5.1-codex",
            "input": [{"type": "message", "role": "user", "content": "ping"}],
            "stream": false
        });
        let core_request =
            llm_proxy_sdk::protocol::client::responses::decode_request(&body).expect("decode");
        let wire = llm_proxy_adapters::ClientWireRequest::new(
            llm_proxy_adapters::ClientWireFormat::OpenAiResponses,
            body,
        );
        let request = EngineRequest::new(
            "sdk-provider",
            core::ProviderRouteKind::Responses,
            llm_proxy_engine::ClientDialect::OpenAiResponses,
            core_request,
            InboundRequest {
                auth: None,
                headers: HashMap::new(),
                wire,
            },
        );
        let outcome = engine.execute(request).await.expect("engine executes");

        // Responses dialect out: encode with the protocol client codec only.
        let response = llm_proxy_sdk::protocol::client::responses::encode_response(&outcome.core)
            .expect("encode");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["text"], "pong");
        assert_eq!(response["usage"]["input_tokens"], 9);
        // The exact usage details survive the cross-format hop.
        let usage_text = serde_json::to_string(&response["usage"]).expect("serialize");
        assert!(
            usage_text.contains("\"cached_tokens\":0"),
            "usage details must never be omitted: {usage_text}"
        );
    }

    #[tokio::test]
    async fn the_gemini_dialect_runs_through_the_engine_without_the_server() {
        let upstream = spawn_fake_upstream().await;
        let config = app_config();
        let provider = core::ProviderConfig {
            name: "sdk-provider".to_owned(),
            api_key: secrecy::SecretString::from("operator-key"),
            auth_style: core::AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "upstream".to_owned(),
                core::ProviderAdapterConfig {
                    protocol: "anthropic_messages".to_owned(),
                    endpoint: format!("{upstream}/v1/messages"),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
                },
            )]),
            routes: core::ProviderRoutesConfig {
                generate_content: Some("upstream".to_owned()),
                ..Default::default()
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
        };
        let engine = ProxyBuilder::new()
            .app_config(config)
            .provider(provider)
            .engine()
            .expect("engine builds");

        let body = json!({
            "contents": [{"role": "user", "parts": [{"text": "ping"}]}]
        });
        let core_request = llm_proxy_sdk::protocol::client::gemini::decode_request(
            &body,
            "gemini-2.5-flash",
            false,
        )
        .expect("decode");
        let wire = llm_proxy_adapters::ClientWireRequest::new(
            llm_proxy_adapters::ClientWireFormat::GeminiGenerateContent,
            body,
        );
        let request = EngineRequest::new(
            "sdk-provider",
            core::ProviderRouteKind::GenerateContent,
            llm_proxy_engine::ClientDialect::Gemini,
            core_request,
            InboundRequest {
                auth: None,
                headers: HashMap::new(),
                wire,
            },
        );
        let outcome = engine.execute(request).await.expect("engine executes");
        let response = llm_proxy_sdk::protocol::client::gemini::encode_response(&outcome.core)
            .expect("encode");
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0]["text"],
            "pong"
        );
        assert_eq!(response["candidates"][0]["finishReason"], "STOP");
        assert_eq!(response["usageMetadata"]["promptTokenCount"], 9);
    }

    #[tokio::test]
    async fn the_ollama_dialect_runs_through_the_engine_without_the_server() {
        let upstream = spawn_fake_upstream().await;
        let config = app_config();
        let provider = core::ProviderConfig {
            name: "sdk-provider".to_owned(),
            api_key: secrecy::SecretString::from("operator-key"),
            auth_style: core::AuthStyle::Bearer,
            passthrough_auth: false,
            api_keys: None,
            retry: None,
            egress: None,
            adapters: HashMap::from([(
                "upstream".to_owned(),
                core::ProviderAdapterConfig {
                    protocol: "anthropic_messages".to_owned(),
                    endpoint: format!("{upstream}/v1/messages"),
                    headers: Arc::new(HashMap::new()),
                    forward_inbound_headers: None,
                    lossy: false,
                },
            )]),
            routes: core::ProviderRoutesConfig {
                chat_completions: Some("upstream".to_owned()),
                ..Default::default()
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
            concurrency: None,
            pricing_sync: None,
            prompt_cache: None,
        };
        let engine = ProxyBuilder::new()
            .app_config(config)
            .provider(provider)
            .engine()
            .expect("engine builds");

        let body = json!({
            "model": "llama3.1",
            "messages": [{"role": "user", "content": "ping"}],
            "stream": false
        });
        let core_request =
            llm_proxy_sdk::protocol::client::ollama::decode_chat_request(&body).expect("decode");
        let wire = llm_proxy_adapters::ClientWireRequest::new(
            llm_proxy_adapters::ClientWireFormat::OllamaChat,
            body,
        );
        let request = EngineRequest::new(
            "sdk-provider",
            core::ProviderRouteKind::ChatCompletions,
            llm_proxy_engine::ClientDialect::Ollama,
            core_request,
            InboundRequest {
                auth: None,
                headers: HashMap::new(),
                wire,
            },
        );
        let outcome = engine.execute(request).await.expect("engine executes");
        let response = llm_proxy_sdk::protocol::client::ollama::encode_chat_response(&outcome.core)
            .expect("encode");
        assert_eq!(response["message"]["content"], "pong");
        assert_eq!(response["done"], true);
        assert_eq!(response["prompt_eval_count"], 9);
        assert_eq!(response["eval_count"], 2);
    }
}
