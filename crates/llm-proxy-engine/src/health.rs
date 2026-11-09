//! Upstream health (plan 04 steps 3, 9, 10; shared across replicas by plan
//! 09 step 1): per-provider and per-credential cooldowns keyed to real
//! reset times, and the sticky-session affinity table. Both live behind
//! the engine so the server, the SDK and the bindings share one view of
//! which member can take the next request.
//!
//! Cooldowns wall-clock times ([`OffsetDateTime`]) so the same view can be
//! held by another process: the default [`CooldownStore`] keeps them in
//! this process, and the optional replica-store crates implement
//! [`Cooldowns`] against a shared store so one replica's cooldown is every
//! replica's cooldown.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

/// Cooldown bookkeeping for providers and individual credentials
/// (backlog 20: passive health tracking from real reset times).
///
/// A cooldown "until" moment is a wall-clock instant, and every method
/// takes `now`, so implementations are not tied to one process's clock.
#[async_trait]
pub trait Cooldowns: std::fmt::Debug + Send + Sync {
    /// Skip `provider` for new attempts until `until`.
    async fn cooldown_provider(&self, provider: &str, until: OffsetDateTime);

    /// Skip credential `credential_id` of `provider` until `until`
    /// (backlog 31: a key that just 429'd or 401'd yields to its
    /// siblings).
    async fn cooldown_credential(&self, provider: &str, credential_id: &str, until: OffsetDateTime);

    /// Ends `provider`'s cooldown now (a successful health probe):
    /// capacity returns immediately instead of when the advertised reset
    /// passes.
    async fn clear_provider(&self, provider: &str);

    /// Whether `provider` is past its cooldown at `now`.
    async fn provider_available(&self, provider: &str, now: OffsetDateTime) -> bool;

    /// The ids of `provider`'s credentials still cooling at `now`, sorted,
    /// for a credential pick to skip while another key exists.
    async fn cooling_credentials(&self, provider: &str, now: OffsetDateTime) -> Vec<String>;

    /// How much longer `provider` stays unavailable at `now`, if it is
    /// cooling.
    async fn provider_remaining(&self, provider: &str, now: OffsetDateTime) -> Option<Duration>;

    /// The soonest moment any of `providers` becomes available at `now`,
    /// ignoring members that never entered cooldown. Used to compute a
    /// `Retry-After` when a whole ladder is exhausted.
    async fn soonest_reset(&self, providers: &[&str], now: OffsetDateTime) -> Option<Duration>;
}

/// The default [`Cooldowns`]: one process's view, behind one lock. A
/// single node runs on it with no external services; a replica deployment
/// swaps in a shared backend through
/// [`EngineBuilder::cooldowns`](crate::EngineBuilder::cooldowns).
#[derive(Debug, Default)]
pub struct CooldownStore {
    providers: Mutex<HashMap<String, OffsetDateTime>>,
    credentials: Mutex<HashMap<(String, String), OffsetDateTime>>,
}

impl CooldownStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Cooldowns for CooldownStore {
    async fn cooldown_provider(&self, provider: &str, until: OffsetDateTime) {
        let mut guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(provider.to_owned()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    async fn cooldown_credential(
        &self,
        provider: &str,
        credential_id: &str,
        until: OffsetDateTime,
    ) {
        let mut guard = self.credentials.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry((provider.to_owned(), credential_id.to_owned()))
            .or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    async fn clear_provider(&self, provider: &str) {
        let mut guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(provider);
    }

    async fn provider_available(&self, provider: &str, now: OffsetDateTime) -> bool {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(provider).is_none_or(|until| now >= *until)
    }

    async fn cooling_credentials(&self, provider: &str, now: OffsetDateTime) -> Vec<String> {
        let guard = self.credentials.lock().unwrap_or_else(|e| e.into_inner());
        let mut cooling: Vec<String> = guard
            .iter()
            .filter(|((owner, _), until)| owner == provider && now < **until)
            .map(|((_, id), _)| id.clone())
            .collect();
        cooling.sort();
        cooling
    }

    async fn provider_remaining(&self, provider: &str, now: OffsetDateTime) -> Option<Duration> {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        let until = guard.get(provider)?;
        (*until - now).try_into().ok()
    }

    async fn soonest_reset(&self, providers: &[&str], now: OffsetDateTime) -> Option<Duration> {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        providers
            .iter()
            .filter_map(|name| guard.get(*name).copied())
            .filter_map(|until| -> Option<std::time::Duration> { (until - now).try_into().ok() })
            .min()
    }
}

/// Sticky-session routing state (plan 04 step 10, backlog 17): a session id
/// maps to the member that last served it, so conversation cache warmth
/// survives across requests while failover still works when that member
/// fails.
///
/// Bounded: when the table is full, the oldest entry (by update) is dropped
/// rather than growing without limit. Single-process, like a local
/// [`CooldownStore`]; a replica deployment that needs shared affinity
/// implements this table in its own store.
#[derive(Debug)]
pub struct AffinityTable {
    sessions: Mutex<HashMap<String, (String, u64)>>,
    capacity: usize,
}

const DEFAULT_AFFINITY_CAPACITY: usize = 4096;

impl Default for AffinityTable {
    fn default() -> Self {
        Self::new(DEFAULT_AFFINITY_CAPACITY)
    }
}

impl AffinityTable {
    /// An empty table holding at most `capacity` session pins.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Record that `session` was last served by `provider`. Called by sticky
    /// pools when an attempt against `provider` is committed to the client;
    /// direct routes and non-sticky pools never pin.
    pub fn pin(&self, session: &str, provider: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() >= self.capacity && !guard.contains_key(session) {
            evict_oldest(&mut guard);
        }
        let tick = guard
            .values()
            .map(|(_, tick)| *tick)
            .max()
            .unwrap_or(0)
            .wrapping_add(1);
        guard.insert(session.to_owned(), (provider.to_owned(), tick));
    }

    /// The member `session` is pinned to, if any.
    #[must_use]
    pub fn pinned_provider(&self, session: &str) -> Option<String> {
        let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(session).map(|(provider, _)| provider.clone())
    }
}

fn evict_oldest(map: &mut HashMap<String, (String, u64)>) {
    let Some(oldest) = map
        .iter()
        .min_by_key(|(_, (_, tick))| *tick)
        .map(|(session, _)| session.clone())
    else {
        return;
    };
    map.remove(&oldest);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fixed base instant per test, so `at(+n)` arithmetic never races
    /// the wall clock.
    fn base() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn at(base: OffsetDateTime, plus_secs: i64) -> OffsetDateTime {
        base + Duration::from_secs(u64::try_from(plus_secs.max(0)).unwrap_or(0))
    }

    #[tokio::test]
    async fn provider_cooldown_blocks_then_expires() {
        let store = CooldownStore::new();
        let now = base();
        store.cooldown_provider("p1", at(now, 10)).await;
        assert!(!store.provider_available("p1", now).await);
        assert!(store.provider_available("p1", at(now, 11)).await);
        assert!(store.provider_available("never-cooled", now).await);
        assert_eq!(
            store.provider_remaining("p1", now).await,
            Some(Duration::from_secs(10))
        );
        assert_eq!(store.provider_remaining("p1", at(now, 60)).await, None);
    }

    #[tokio::test]
    async fn cooldown_extends_never_shortens() {
        let store = CooldownStore::new();
        let now = base();
        store.cooldown_provider("p", at(now, 60)).await;
        store.cooldown_provider("p", at(now, 5)).await;
        assert!(
            !store.provider_available("p", at(now, 30)).await,
            "a shorter later cooldown must not shorten an active one"
        );
    }

    #[tokio::test]
    async fn credential_cooldowns_are_per_key() {
        let store = CooldownStore::new();
        let now = base();
        store.cooldown_credential("p", "key-0", at(now, 10)).await;
        assert_eq!(
            store.cooling_credentials("p", now).await,
            vec!["key-0".to_owned()],
            "only the rejected key is cooling"
        );
        assert!(
            store.cooling_credentials("p", at(now, 11)).await.is_empty(),
            "the cooldown passes"
        );
        assert!(
            store.cooling_credentials("other", now).await.is_empty(),
            "cooldowns are per provider"
        );
    }

    #[tokio::test]
    async fn clear_ends_a_cooldown_immediately() {
        let store = CooldownStore::new();
        let now = base();
        store.cooldown_provider("p", at(now, 600)).await;
        assert!(!store.provider_available("p", now).await);
        store.clear_provider("p").await;
        assert!(store.provider_available("p", now).await);
    }

    #[tokio::test]
    async fn soonest_reset_reports_the_minimum_wait() {
        let store = CooldownStore::new();
        let now = base();
        store.cooldown_provider("a", at(now, 30)).await;
        store.cooldown_provider("b", at(now, 5)).await;
        assert_eq!(
            store.soonest_reset(&["a", "b", "c"], now).await,
            Some(Duration::from_secs(5))
        );
        assert_eq!(store.soonest_reset(&["c"], now).await, None);
    }

    #[test]
    fn affinity_pins_and_retrieves_the_serving_member() {
        let table = AffinityTable::new(16);
        assert_eq!(table.pinned_provider("sess-1"), None);
        table.pin("sess-1", "primary");
        assert_eq!(table.pinned_provider("sess-1").as_deref(), Some("primary"));
        table.pin("sess-1", "secondary");
        assert_eq!(
            table.pinned_provider("sess-1").as_deref(),
            Some("secondary"),
            "the latest serving member wins"
        );
    }

    #[test]
    fn affinity_table_evicts_when_full_instead_of_growing() {
        let table = AffinityTable::new(2);
        table.pin("s1", "a");
        table.pin("s2", "b");
        table.pin("s3", "c");
        assert_eq!(
            table.pinned_provider("s1"),
            None,
            "the oldest pin is dropped"
        );
        assert_eq!(table.pinned_provider("s3").as_deref(), Some("c"));
    }
}
