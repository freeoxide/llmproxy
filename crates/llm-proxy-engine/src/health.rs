//! Process-shared upstream health (plan 04 steps 3, 9, 10): per-provider and
//! per-credential cooldowns keyed to real reset times, and the sticky-session
//! affinity table. Both live behind the engine so the server, the SDK and
//! the bindings share one view of which member can take the next request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cooldown bookkeeping for providers and individual credentials
/// (backlog 20: passive health tracking from real reset times).
///
/// `Instant`-based (single-process view). A future replica deployment moves
/// this behind the same methods backed by Redis; the engine only talks to
/// this type.
#[derive(Debug, Default)]
pub struct CooldownStore {
    providers: Mutex<HashMap<String, Instant>>,
    credentials: Mutex<HashMap<(String, String), Instant>>,
}

impl CooldownStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Skip `provider` for new attempts until `until`.
    pub fn cooldown_provider(&self, provider: &str, until: Instant) {
        let mut guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(provider.to_owned()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Skip credential `credential_id` of `provider` until `until`
    /// (backlog 31: a key that just 429'd or 401'd yields to its siblings).
    pub fn cooldown_credential(&self, provider: &str, credential_id: &str, until: Instant) {
        let mut guard = self.credentials.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry((provider.to_owned(), credential_id.to_owned()))
            .or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Ends `provider`'s cooldown now (a successful health probe): capacity
    /// returns immediately instead of when the advertised reset passes.
    pub fn clear_provider(&self, provider: &str) {
        let mut guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(provider);
    }

    /// Whether `provider` is past its cooldown.
    #[must_use]
    pub fn provider_available(&self, provider: &str, now: Instant) -> bool {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(provider).is_none_or(|until| now >= *until)
    }

    /// The ids of `provider`'s credentials still cooling at `now`, sorted,
    /// for a credential pick to skip while another key exists.
    #[must_use]
    pub fn cooling_credentials(&self, provider: &str, now: Instant) -> Vec<String> {
        let guard = self.credentials.lock().unwrap_or_else(|e| e.into_inner());
        let mut cooling: Vec<String> = guard
            .iter()
            .filter(|((owner, _), until)| owner == provider && now < **until)
            .map(|((_, id), _)| id.clone())
            .collect();
        cooling.sort();
        cooling
    }

    /// How much longer `provider` stays unavailable, if it is cooling.
    #[must_use]
    pub fn provider_remaining(&self, provider: &str, now: Instant) -> Option<Duration> {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(provider)
            .and_then(|until| until.checked_duration_since(now))
    }

    /// The soonest moment any of `providers` becomes available, ignoring
    /// members that never entered cooldown. Used to compute a `Retry-After`
    /// when a whole ladder is exhausted.
    #[must_use]
    pub fn soonest_reset<'a>(
        &self,
        providers: impl IntoIterator<Item = &'a str>,
        now: Instant,
    ) -> Option<Duration> {
        let guard = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        providers
            .into_iter()
            .filter_map(|name| guard.get(name))
            .filter_map(|until| until.checked_duration_since(now))
            .min()
    }
}

/// Sticky-session routing state (plan 04 step 10, backlog 17): a session id
/// maps to the member that last served it, so conversation cache warmth
/// survives across requests while failover still works when that member
/// fails.
///
/// Bounded: when the table is full, the oldest entry (by update) is dropped
/// rather than growing without limit. Single-process, like
/// [`CooldownStore`].
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

    #[test]
    fn provider_cooldown_blocks_then_expires() {
        let store = CooldownStore::new();
        let now = Instant::now();
        store.cooldown_provider("p1", now + Duration::from_secs(10));
        assert!(!store.provider_available("p1", now));
        assert!(store.provider_available("p1", now + Duration::from_secs(11)));
        assert!(store.provider_available("never-cooled", now));
        assert_eq!(
            store.provider_remaining("p1", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            store.provider_remaining("p1", now + Duration::from_secs(60)),
            None
        );
    }

    #[test]
    fn cooldown_extends_never_shortens() {
        let store = CooldownStore::new();
        let now = Instant::now();
        store.cooldown_provider("p", now + Duration::from_secs(60));
        store.cooldown_provider("p", now + Duration::from_secs(5));
        assert!(
            !store.provider_available("p", now + Duration::from_secs(30)),
            "a shorter later cooldown must not shorten an active one"
        );
    }

    #[test]
    fn credential_cooldowns_are_per_key() {
        let store = CooldownStore::new();
        let now = Instant::now();
        store.cooldown_credential("p", "key-0", now + Duration::from_secs(10));
        assert_eq!(
            store.cooling_credentials("p", now),
            vec!["key-0".to_owned()],
            "only the rejected key is cooling"
        );
        assert!(
            store
                .cooling_credentials("p", now + Duration::from_secs(11))
                .is_empty(),
            "the cooldown passes"
        );
        assert!(
            store.cooling_credentials("other", now).is_empty(),
            "cooldowns are per provider"
        );
    }

    #[test]
    fn soonest_reset_reports_the_minimum_wait() {
        let store = CooldownStore::new();
        let now = Instant::now();
        store.cooldown_provider("a", now + Duration::from_secs(30));
        store.cooldown_provider("b", now + Duration::from_secs(5));
        assert_eq!(
            store.soonest_reset(["a", "b", "c"], now),
            Some(Duration::from_secs(5))
        );
        assert_eq!(store.soonest_reset(["c"], now), None);
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
