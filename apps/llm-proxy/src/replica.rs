//! Replica-store wiring (plan 09 step 1, audit conflict 13): when
//! `[replica]` is configured, the daemon's admission step, cooldown view
//! and usage feed move to the shared store, so every replica pointing at
//! it enforces one policy. The optional crates are behind cargo features;
//! a configured `[replica]` in a build without the matching feature is a
//! startup error, never a silent single-node fallback.

use std::sync::Arc;

use anyhow::{Context, Result};
use llm_proxy_core::{AppConfig, ProviderRegistry, ReplicaBackend};
use llm_proxy_server::AppState;
use secrecy::ExposeSecret;

/// Applies `[replica]` to `state`, returning the replacement state.
///
/// # Errors
///
/// `anyhow` when `[replica]` names a backend this build cannot serve (the
/// feature is off), or connecting to the shared store fails.
pub async fn apply_replica(
    config: &AppConfig,
    registry: &ProviderRegistry,
    state: AppState,
) -> Result<AppState> {
    let Some(replica) = config.replica.as_ref() else {
        return Ok(state);
    };
    let url = replica.url.expose_secret();
    let auth = &config.auth;
    match replica.backend {
        ReplicaBackend::Redis => {
            #[cfg(feature = "store-redis")]
            {
                let store = llm_proxy_store_redis::RedisStore::connect(url, &replica.prefix)
                    .await
                    .with_context(|| "connecting to the Redis replica store from [replica]")?;
                let admission = llm_proxy_store_redis::replica_admission_over_config(
                    auth,
                    registry,
                    store.clone(),
                );
                let cooldowns = Arc::new(llm_proxy_store_redis::RedisCooldowns::new(store.clone()));
                let bus = Arc::new(llm_proxy_store_redis::RedisEventBus::new(store, "usage"));
                tracing::info!(
                    prefix = %replica.prefix,
                    "replica mode: admission, cooldowns and usage shared through Redis"
                );
                Ok(state
                    .with_admission(Arc::new(admission))
                    .with_cooldowns(cooldowns)
                    .with_event_bus(bus))
            }
            #[cfg(not(feature = "store-redis"))]
            {
                let _ = (url, auth, registry, state);
                bail!(
                    "[replica] backend \"redis\" is configured but this build lacks the \
                     store-redis feature; rebuild with --features store-redis"
                );
            }
        }
        ReplicaBackend::Postgres => {
            #[cfg(feature = "store-postgres")]
            {
                let store = llm_proxy_store_postgres::PostgresStore::connect(url)
                    .await
                    .with_context(|| "connecting to the Postgres replica store from [replica]")?;
                spawn_purge_timer(store.clone());
                let admission = llm_proxy_store_postgres::replica_admission_over_config(
                    auth,
                    registry,
                    store.clone(),
                );
                let cooldowns = Arc::new(llm_proxy_store_postgres::PostgresCooldowns::new(
                    store.clone(),
                ));
                let bus = Arc::new(llm_proxy_store_postgres::PostgresEventBus::new(store));
                tracing::info!(
                    "replica mode: admission, cooldowns and usage shared through Postgres"
                );
                Ok(state
                    .with_admission(Arc::new(admission))
                    .with_cooldowns(cooldowns)
                    .with_event_bus(bus))
            }
            #[cfg(not(feature = "store-postgres"))]
            {
                let _ = (url, auth, registry, state);
                bail!(
                    "[replica] backend \"postgres\" is configured but this build lacks the \
                     store-postgres feature; rebuild with --features store-postgres"
                );
            }
        }
    }
}

/// The Postgres store keeps stale window rows until purged; a timer bounds
/// them (Redis keys expire on their own TTLs).
#[cfg(feature = "store-postgres")]
fn spawn_purge_timer(store: llm_proxy_store_postgres::PostgresStore) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match store.purge_expired().await {
                Ok(purged) if purged > 0 => {
                    tracing::debug!(purged, "replica store purge removed stale rows");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "replica store purge failed");
                }
            }
        }
    });
}
