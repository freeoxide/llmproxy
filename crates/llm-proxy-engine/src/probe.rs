//! The optional synthetic health probe (plan 04 step 9, backlog 20):
//! periodically sends a minimal one-token request to cooled-down pool
//! members; a healthy answer clears the cooldown early, so capacity returns
//! as soon as the upstream recovers instead of when the advertised reset
//! happens to pass.
//!
//! Passive health (429 and rate-limit headers into real-reset cooldowns) is
//! always on — see [`crate::health::CooldownStore`]. The probe is opt-in per
//! pool via `[pool.health] probe_interval`.

use std::sync::Arc;

use llm_proxy_core::ProviderRouteKind;
use llm_proxy_protocol::core::{
    CoreContent, CoreMessage, CoreRequest, CoreRole, ModelRef, SamplingOptions,
};

use crate::error::map_provider_error;
use crate::lifecycle::probe_credential_pool;
use crate::{EngineError, EngineInner};

/// Spawns the probe loops for every pool with `probe_interval` configured.
/// Called at engine build; only spawns when a tokio runtime is running (an
/// embedder building an engine outside a runtime gets no probes — the same
/// engine built inside one does). The tasks run for the engine's lifetime.
pub(crate) fn spawn_pool_probes(inner: &Arc<EngineInner>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime at engine build; pool health probes are not running");
        return;
    };
    for pool in inner.providers.pools() {
        let Some(interval) = pool.health.probe_interval else {
            continue;
        };
        let inner = Arc::clone(inner);
        let pool_name = pool.name.clone();
        let members = pool.members.clone();
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                probe_members(&inner, &pool_name, &members).await;
            }
        });
    }
}

/// Probes every cooled-down member of one pool: a successful synthetic
/// request clears the member's cooldown; a failure leaves the existing
/// cooldown standing, to expire at its advertised reset (the probe never
/// lengthens a cooldown — only real traffic through the ladder does).
async fn probe_members(inner: &Arc<EngineInner>, pool_name: &str, members: &[String]) {
    for member in members {
        if inner
            .health
            .provider_available(member, time::OffsetDateTime::now_utc())
            .await
        {
            // Healthy members are never probed: probes cost a request.
            continue;
        }
        match probe_member(inner, member).await {
            Ok(()) => {
                tracing::info!(
                    pool = %pool_name,
                    member = %member,
                    "health probe succeeded; clearing cooldown"
                );
                inner.health.clear_provider(member).await;
            }
            Err(error) => {
                tracing::debug!(
                    pool = %pool_name,
                    member = %member,
                    error = %error,
                    "health probe failed; cooldown stands"
                );
            }
        }
    }
}

/// Sends one synthetic one-token request to `member` through its configured
/// messages route. `Ok(())` means the upstream answered 2xx.
///
/// The probe needs a model the member actually serves; it takes the first
/// configured alias target (else the first static catalog entry). A member
/// with neither is not probed — there is no honest synthetic request to
/// send, so its cooldowns simply expire.
async fn probe_member(inner: &Arc<EngineInner>, member: &str) -> Result<(), EngineError> {
    let Some(model) = probe_model_for(inner, member) else {
        tracing::debug!(
            member = %member,
            "health probe skipped: no alias target or catalog entry names a servable model"
        );
        return Ok(());
    };
    let target = inner
        .providers
        .resolve_provider_route(member, ProviderRouteKind::Messages, &model)
        .map_err(EngineError::from_route_error)?;
    let adapter = inner.adapters.get(&target.protocol).ok_or_else(|| {
        EngineError::Internal(format!(
            "no adapter registered for protocol: {}",
            target.protocol
        ))
    })?;
    let keys = probe_credential_pool(inner, member);
    let cooling = inner
        .health
        .cooling_credentials(member, time::OffsetDateTime::now_utc())
        .await;
    let credential = inner
        .credentials
        .pick(&crate::traits::CredentialQuery {
            provider: member,
            keys: &keys,
            passthrough: false,
            inbound: None,
            avoid: None,
            cooling: &cooling,
        })
        .await?;

    let core = probe_request(&model, &target.upstream_model);
    let wire = llm_proxy_adapters::client_wire::ClientWireRequest::new(
        llm_proxy_adapters::client_wire::ClientWireFormat::AnthropicMessages,
        serde_json::json!({}),
    );
    let request = adapter
        .encode_request(
            &core,
            &llm_proxy_adapters::ProviderAdapterTarget {
                provider_name: target.provider_name.clone(),
                adapter_name: target.adapter_name.clone(),
                protocol: llm_proxy_adapters::ProtocolName::new(&target.protocol),
                endpoint: target.endpoint.clone(),
                auth_style: target.auth_style,
                api_key: credential.secret,
                requested_model: target.requested_model.clone(),
                upstream_model: target.upstream_model.clone(),
                headers: Arc::clone(&target.headers),
                lossy: target.lossy,
                forward_inbound_headers: Arc::clone(&target.forward_inbound_headers),
                prompt_cache: target.prompt_cache.clone(),
            },
            &wire,
        )
        .map_err(|e| map_probe_encode_error(&e))?;
    inner
        .transport
        .client_for(member)
        .send(request)
        .await
        .map(|_| ())
        .map_err(|error| map_provider_error(&error, false))
}

/// A model the member serves: its first alias target, else its first static
/// catalog entry.
fn probe_model_for(inner: &EngineInner, member: &str) -> Option<String> {
    let provider = inner.providers.get(member)?;
    if let Some(target) = provider.model_aliases.values().next() {
        return Some(target.clone());
    }
    provider
        .catalog
        .as_ref()
        .and_then(|catalog| catalog.models.first().map(|entry| entry.id.clone()))
}

/// Encode failures in a probe are configuration facts, not verdicts about
/// the member's health: surface them as internal errors.
fn map_probe_encode_error(e: &llm_proxy_adapters::ProviderError) -> EngineError {
    EngineError::Internal(format!("probe encode error: {e}"))
}

/// The minimal request a probe sends: one user turn, one token, no stream.
fn probe_request(model: &str, upstream_model: &str) -> CoreRequest {
    CoreRequest {
        model: ModelRef {
            requested: model.to_owned(),
            upstream: Some(upstream_model.to_owned()),
        },
        messages: vec![CoreMessage {
            role: CoreRole::User,
            content: vec![CoreContent::Text {
                text: "ping".to_owned(),
                cache: None,
            }],
        }],
        system: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        sampling: SamplingOptions {
            max_tokens: Some(1),
            ..SamplingOptions::default()
        },
        stream: false,
        metadata: Default::default(),
        provider_hints: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{CooldownStore, Cooldowns};
    use std::time::Duration;

    #[test]
    fn probe_requests_ask_for_one_token_without_streaming() {
        let request = probe_request("m", "m-upstream");
        assert_eq!(request.sampling.max_tokens, Some(1));
        assert!(!request.stream);
        assert_eq!(request.model.upstream.as_deref(), Some("m-upstream"));
    }

    #[tokio::test]
    async fn clearing_a_cooldown_makes_the_member_available_again() {
        let store = CooldownStore::new();
        let now = time::OffsetDateTime::now_utc();
        store
            .cooldown_provider("member", now + Duration::from_secs(600))
            .await;
        assert!(!store.provider_available("member", now).await);
        // What a successful probe does:
        store.clear_provider("member").await;
        assert!(
            store.provider_available("member", now).await,
            "a probe success restores capacity immediately"
        );
    }
}
