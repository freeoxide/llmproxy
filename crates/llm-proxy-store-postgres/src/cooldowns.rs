//! Shared upstream cooldowns in Postgres (plan 09 step 1): one replica's
//! cooldown is every replica's cooldown, stored with wall-clock resets.

use std::time::Duration;

use async_trait::async_trait;
use llm_proxy_engine::Cooldowns;
use time::OffsetDateTime;

use tokio_postgres::types::ToSql;

use crate::PostgresStore;

fn provider_scope(provider: &str) -> String {
    format!("provider:{provider}")
}

fn credential_scope(provider: &str) -> String {
    format!("credential:{provider}")
}

/// The Postgres-backed [`Cooldowns`]: `GREATEST` upserts keep an existing
/// later reset standing, so a cooldown extends and never shortens.
#[derive(Debug, Clone)]
pub struct PostgresCooldowns {
    store: PostgresStore,
}

impl PostgresCooldowns {
    /// Over a connected store.
    #[must_use]
    pub fn new(store: PostgresStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Cooldowns for PostgresCooldowns {
    async fn cooldown_provider(&self, provider: &str, until: OffsetDateTime) {
        let sql = "INSERT INTO cooldowns (scope, name, until) VALUES ($1, '', $2) \
                   ON CONFLICT (scope, name) DO UPDATE \
                   SET until = GREATEST(cooldowns.until, EXCLUDED.until)";
        let Ok(client) = self.store.connection().await else {
            tracing::warn!("opening a Postgres connection for a cooldown write failed");
            return;
        };
        if let Err(error) = client
            .execute(
                sql,
                &[&provider_scope(provider) as &(dyn ToSql + Sync), &until],
            )
            .await
        {
            tracing::warn!(%error, "writing provider cooldown to Postgres failed");
        }
    }

    async fn cooldown_credential(
        &self,
        provider: &str,
        credential_id: &str,
        until: OffsetDateTime,
    ) {
        let sql = "INSERT INTO cooldowns (scope, name, until) VALUES ($1, $2, $3) \
                   ON CONFLICT (scope, name) DO UPDATE \
                   SET until = GREATEST(cooldowns.until, EXCLUDED.until)";
        let Ok(client) = self.store.connection().await else {
            tracing::warn!("opening a Postgres connection for a cooldown write failed");
            return;
        };
        if let Err(error) = client
            .execute(
                sql,
                &[
                    &credential_scope(provider) as &(dyn ToSql + Sync),
                    &credential_id,
                    &until,
                ],
            )
            .await
        {
            tracing::warn!(%error, "writing credential cooldown to Postgres failed");
        }
    }

    async fn clear_provider(&self, provider: &str) {
        let sql = "DELETE FROM cooldowns WHERE scope = $1";
        let Ok(client) = self.store.connection().await else {
            tracing::warn!("opening a Postgres connection for a cooldown clear failed");
            return;
        };
        if let Err(error) = client
            .execute(sql, &[&provider_scope(provider) as &(dyn ToSql + Sync)])
            .await
        {
            tracing::warn!(%error, "clearing provider cooldown in Postgres failed");
        }
    }

    async fn provider_available(&self, provider: &str, now: OffsetDateTime) -> bool {
        let sql = "SELECT NOT EXISTS (SELECT 1 FROM cooldowns WHERE scope = $1 AND until > $2)";
        let Ok(client) = self.store.connection().await else {
            tracing::warn!("opening a Postgres connection failed; treating member as available");
            return true;
        };
        match client
            .query_opt(
                sql,
                &[&provider_scope(provider) as &(dyn ToSql + Sync), &now],
            )
            .await
        {
            Ok(Some(row)) => row.get(0),
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(%error, "reading provider cooldown failed; treating as available");
                true
            }
        }
    }

    async fn cooling_credentials(&self, provider: &str, now: OffsetDateTime) -> Vec<String> {
        let sql = "SELECT name FROM cooldowns WHERE scope = $1 AND until > $2 ORDER BY name";
        let Ok(client) = self.store.connection().await else {
            tracing::warn!("opening a Postgres connection for credential cooldowns failed");
            return Vec::new();
        };
        match client
            .query(
                sql,
                &[&credential_scope(provider) as &(dyn ToSql + Sync), &now],
            )
            .await
        {
            Ok(rows) => rows.iter().map(|row| row.get(0)).collect(),
            Err(error) => {
                tracing::warn!(%error, "reading credential cooldowns failed");
                Vec::new()
            }
        }
    }

    async fn provider_remaining(&self, provider: &str, now: OffsetDateTime) -> Option<Duration> {
        let sql = "SELECT until FROM cooldowns WHERE scope = $1 AND until > $2";
        let client = self.store.connection().await.ok()?;
        let row = client
            .query_opt(
                sql,
                &[&provider_scope(provider) as &(dyn ToSql + Sync), &now],
            )
            .await
            .ok()??;
        let until: OffsetDateTime = row.get(0);
        let millis = (until - now).whole_milliseconds().max(0);
        (millis > 0).then(|| Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
    }

    async fn soonest_reset(&self, providers: &[&str], now: OffsetDateTime) -> Option<Duration> {
        let client = self.store.connection().await.ok()?;
        let sql = "SELECT until FROM cooldowns WHERE scope = $1 AND until > $2";
        let mut soonest: Option<OffsetDateTime> = None;
        for provider in providers {
            let row = client
                .query_opt(
                    sql,
                    &[&provider_scope(provider) as &(dyn ToSql + Sync), &now],
                )
                .await
                .ok()??;
            let until: OffsetDateTime = row.get(0);
            if soonest.is_none_or(|best| until < best) {
                soonest = Some(until);
            }
        }
        soonest.map(|until| {
            let millis = (until - now).whole_milliseconds().max(0);
            Duration::from_millis(u64::try_from(millis).unwrap_or(0))
        })
    }
}
