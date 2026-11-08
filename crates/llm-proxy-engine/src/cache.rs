//! The engine's in-process sharing and response stores (plan 06): the
//! opt-in exact-match response cache, the per-caller single-flight
//! registries that merge identical concurrent requests into one upstream
//! call, and the opt-in `Idempotency-Key` store that replays a stored
//! response for a matching retry.
//!
//! All three key on the caller, so one caller's stored or shared response
//! can never be handed to another (audit conflict 7). None of them persists
//! anything: a replica-safe backend is a later optional crate implementing
//! the same traits.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use llm_proxy_core::ProviderRouteKind;
use llm_proxy_protocol::core::CoreEvent;
use tokio::sync::watch;

use crate::error::EngineError;
use crate::traits::{Cache, CacheEntry, CacheKey};
use crate::types::EngineResponse;

/// An owned [`CacheKey`]: the exact-match identity of one normalized request,
/// caller-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheIdentity {
    caller: String,
    provider: String,
    route_kind: ProviderRouteKind,
    model: String,
    body_hash: String,
    streaming: bool,
}

impl CacheIdentity {
    pub(crate) fn from_key(key: &CacheKey<'_>) -> Self {
        Self {
            caller: key.caller.to_owned(),
            provider: key.provider.to_owned(),
            route_kind: key.route_kind,
            model: key.model.to_owned(),
            body_hash: key.body_hash.to_owned(),
            streaming: key.streaming,
        }
    }
}

/// The in-process [`Cache`] (plan 06 step 4): exact-match, caller-scoped,
/// bounded, TTL-aged. Completed non-streaming responses only — streaming
/// requests are shared while in flight by [`StreamShares`], never stored.
#[derive(Debug)]
pub struct MemoryCache {
    inner: Mutex<CacheTable>,
    ttl: Duration,
    max_entries: usize,
}

#[derive(Debug, Default)]
struct CacheTable {
    entries: HashMap<CacheIdentity, (CacheEntry, Instant, u64)>,
    /// Insertion order with each insert's generation, for FIFO eviction at
    /// the capacity bound. A re-stored identity gets a new generation, so
    /// its older queue slot cannot evict the newer entry.
    order: VecDeque<(CacheIdentity, u64)>,
    next_generation: u64,
}

impl MemoryCache {
    /// A cache holding entries for `ttl`, at most `max_entries` (the oldest
    /// stored is evicted first).
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(CacheTable::default()),
            ttl,
            max_entries,
        }
    }

    fn expired(&self, stored_at: Instant) -> bool {
        stored_at.elapsed() >= self.ttl
    }

    /// Drops expired entries and their order slots, and reclaims superseded
    /// slots. Stored-at times are non-decreasing along the queue (a re-stored
    /// identity re-enqueues at the back with a fresh clock), so every expired
    /// entry sits at the front: without this sweep the queue would keep one
    /// dead slot per expired identity and grow without bound under churn.
    fn sweep_expired(&self, table: &mut CacheTable) {
        while let Some((identity, generation)) = table.order.front().cloned() {
            let current = table.entries.get(&identity).map(|(_, _, live)| *live);
            match current {
                Some(live) if live == generation => {
                    let expired = table
                        .entries
                        .get(&identity)
                        .is_some_and(|(_, stored_at, _)| self.expired(*stored_at));
                    if !expired {
                        break;
                    }
                    table.entries.remove(&identity);
                    table.order.pop_front();
                }
                // A superseded slot: its identity was re-stored under a newer
                // generation (or removed); the slot alone goes.
                _ => {
                    table.order.pop_front();
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Cache for MemoryCache {
    async fn lookup(&self, key: &CacheKey<'_>) -> Result<Option<CacheEntry>, EngineError> {
        let identity = CacheIdentity::from_key(key);
        let mut table = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep_expired(&mut table);
        match table.entries.get(&identity) {
            Some((entry, stored_at, _)) if !self.expired(*stored_at) => Ok(Some(entry.clone())),
            // Unreachable after the sweep (every remaining entry is live);
            // kept so an expired hit can never be served even if that
            // invariant changes.
            Some(_) => {
                table.entries.remove(&identity);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn store(&self, key: &CacheKey<'_>, entry: &CacheEntry) {
        let identity = CacheIdentity::from_key(key);
        let mut table = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep_expired(&mut table);
        let generation = table.next_generation;
        table.next_generation += 1;
        table.entries.insert(
            identity.clone(),
            (entry.clone(), Instant::now(), generation),
        );
        table.order.push_back((identity, generation));
        while table.entries.len() > self.max_entries {
            let Some((candidate, candidate_generation)) = table.order.pop_front() else {
                break;
            };
            let current = table
                .entries
                .get(&candidate)
                .map(|(_, _, generation)| *generation);
            if current == Some(candidate_generation) {
                table.entries.remove(&candidate);
            }
        }
    }
}

/// The outcome a single-flight follower receives: the leader's response, or
/// the leader's error.
pub(crate) type FlightOutcome = Result<EngineResponse, EngineError>;

/// What acquiring a flight returned.
pub(crate) enum FlightAcquire {
    /// No identical request is running: this request leads, and must publish
    /// an outcome exactly once (dropping the lead unpublished publishes an
    /// internal error, so followers can never hang).
    Lead(FlightLead),
    /// An identical request from the same caller is running: wait for its
    /// outcome.
    Join(FlightFollower),
}

/// The leader's handle to one in-flight request.
pub(crate) struct FlightLead {
    registry: Arc<SingleFlight>,
    identity: CacheIdentity,
    share: Arc<FlightShare>,
    published: bool,
}

impl FlightLead {
    /// Publish the outcome to every follower and release the claim. Called
    /// exactly once per lead.
    pub(crate) fn publish(mut self, outcome: FlightOutcome) {
        self.published = true;
        let _ = self.share.tx.send_replace(Some(outcome));
        self.registry.release(&self.identity, &self.share);
    }
}

/// The message the [`FlightLead`] drop backstop publishes: the leader ended
/// unpublished (its handler future was dropped on a client disconnect, or it
/// panicked). Followers match on it to re-execute the request themselves
/// instead of failing with an internal error.
pub(crate) const FLIGHT_LEADER_LOST: &str = "single-flight leader ended without an outcome";

/// Whether `error` is the single-flight leader-loss backstop, which a
/// follower answers by executing the request itself.
pub(crate) fn is_flight_leader_lost(error: &EngineError) -> bool {
    matches!(error, EngineError::Internal(message) if message == FLIGHT_LEADER_LOST)
}

/// The message a stream leader publishes when it goes away before its
/// stream committed (its caller's connection dropped, or the task died):
/// the joiner's first-byte wait resolves with it, and the joiner recognizes
/// it to re-run the request itself instead of failing with an internal
/// error. A stream that already committed never publishes this — its
/// joiners are already replaying its collected events.
pub(crate) const STREAM_LEADER_LOST: &str = "shared stream leader went away before its first byte";

/// Whether `error` is the stream leader-loss sentinel, which a joiner
/// answers by re-running the request itself.
pub(crate) fn is_stream_leader_lost(error: &EngineError) -> bool {
    matches!(error, EngineError::Internal(message) if message == STREAM_LEADER_LOST)
}

impl Drop for FlightLead {
    fn drop(&mut self) {
        if !self.published {
            // The leader ended without publishing (a dropped handler future
            // or a panic): fail the followers rather than leaving them
            // waiting forever. They recognize this sentinel and re-execute.
            let _ = self.share.tx.send_replace(Some(Err(EngineError::Internal(
                FLIGHT_LEADER_LOST.to_owned(),
            ))));
            self.registry.release(&self.identity, &self.share);
        }
    }
}

/// A follower's subscription to the leader's outcome.
pub(crate) struct FlightFollower {
    rx: watch::Receiver<Option<FlightOutcome>>,
}

impl FlightFollower {
    /// Wait for the leader's outcome. Resolves immediately when the leader
    /// already finished.
    pub(crate) async fn wait(self) -> FlightOutcome {
        let mut rx = self.rx;
        loop {
            if let Some(outcome) = rx.borrow_and_update().clone() {
                return outcome;
            }
            if rx.changed().await.is_err() {
                // Every sender dropped without publishing; the lead's Drop
                // backstop holds the last sender until it publishes, so this
                // is unreachable in practice.
                return Err(EngineError::Internal(
                    "single-flight leader vanished without an outcome".to_owned(),
                ));
            }
        }
    }
}

struct FlightShare {
    tx: watch::Sender<Option<FlightOutcome>>,
}

/// Per-caller single-flight for non-streaming requests (plan 06 step 2):
/// identical concurrent requests from one caller share one upstream call —
/// the first leads, the rest await its outcome. Nothing is retained after
/// the outcome is published.
#[derive(Default)]
pub struct SingleFlight {
    flights: Mutex<HashMap<CacheIdentity, Arc<FlightShare>>>,
}

impl std::fmt::Debug for SingleFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The count, never the outcomes: responses must not reach logs.
        let len = self.flights.lock().map_or(0, |flights| flights.len());
        f.debug_struct("SingleFlight")
            .field("in_flight", &len)
            .finish()
    }
}

impl SingleFlight {
    /// A fresh registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or join the flight for `key`.
    pub(crate) fn acquire(self: &Arc<Self>, key: &CacheKey<'_>) -> FlightAcquire {
        let identity = CacheIdentity::from_key(key);
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(share) = flights.get(&identity) {
            return FlightAcquire::Join(FlightFollower {
                rx: share.tx.subscribe(),
            });
        }
        let (tx, _rx) = watch::channel(None);
        let share = Arc::new(FlightShare { tx });
        flights.insert(identity.clone(), Arc::clone(&share));
        FlightAcquire::Lead(FlightLead {
            registry: Arc::clone(self),
            identity,
            share,
            published: false,
        })
    }

    fn release(&self, identity: &CacheIdentity, share: &Arc<FlightShare>) {
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        if flights
            .get(identity)
            .is_some_and(|registered| Arc::ptr_eq(registered, share))
        {
            flights.remove(identity);
        }
    }
}

/// The outcome a stream leader publishes when its stream ends.
pub(crate) type StreamOutcome = Result<(), EngineError>;

/// What acquiring a stream share returned.
pub(crate) enum StreamAcquire {
    /// This request leads and must publish once its stream ends.
    Lead(StreamLead),
    /// An identical streaming request is running: replay its collected
    /// events once it finishes.
    Join(Arc<StreamShare>),
}

/// One shared streaming request: the events the leader has forwarded so far,
/// whether its stream committed to a client, and the terminal outcome.
pub(crate) struct StreamShare {
    events: Mutex<Vec<CoreEvent>>,
    /// The first-byte boundary: `Ok` once the leader committed its stream
    /// (joiners may answer 200 and replay), or the leader's error when it
    /// failed before any byte was committed (joiners fail the same way).
    commit: watch::Sender<Option<Result<(), EngineError>>>,
    tx: watch::Sender<Option<StreamOutcome>>,
}

impl StreamShare {
    /// Append one event the leader forwarded (its tap point).
    pub(crate) fn record(&self, event: &CoreEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }

    /// The events collected so far, in order.
    pub(crate) fn collected(&self) -> Vec<CoreEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Marks the share's stream as committed: joiners may answer 200 and
    /// replay its collected events.
    pub(crate) fn signal_committed(&self) {
        self.commit.send_if_modified(|slot| {
            if slot.is_none() {
                *slot = Some(Ok(()));
                true
            } else {
                false
            }
        });
    }

    /// Fails the first-byte wait when the leader ends without committing, so
    /// a joiner sees the leader's error instead of waiting or streaming a
    /// generic one. A stream that already committed keeps its `Ok`.
    pub(crate) fn fail_commit_if_pending(&self, error: &EngineError) {
        self.commit.send_if_modified(|slot| {
            if slot.is_none() {
                *slot = Some(Err(error.clone()));
                true
            } else {
                false
            }
        });
    }

    /// Wait for the leader's commit outcome: `Ok` once its stream committed,
    /// or its error when it failed before the first byte. Resolves
    /// immediately when the leader already crossed the boundary.
    pub(crate) async fn committed(&self) -> Result<(), EngineError> {
        let mut rx = self.commit.subscribe();
        loop {
            if let Some(outcome) = rx.borrow_and_update().clone() {
                return outcome;
            }
            if rx.changed().await.is_err() {
                return Err(EngineError::Internal(
                    "shared stream leader vanished before its first byte".to_owned(),
                ));
            }
        }
    }

    /// Wait for the leader's terminal outcome. Resolves immediately when the
    /// leader already finished.
    pub(crate) async fn outcome(&self) -> StreamOutcome {
        let mut rx = self.tx.subscribe();
        loop {
            if let Some(outcome) = rx.borrow_and_update().clone() {
                return outcome;
            }
            if rx.changed().await.is_err() {
                return Err(EngineError::Internal(
                    "shared stream leader vanished without an outcome".to_owned(),
                ));
            }
        }
    }
}

/// The leader's handle to one shared streaming request.
pub(crate) struct StreamLead {
    registry: Arc<StreamShares>,
    identity: CacheIdentity,
    share: Arc<StreamShare>,
    published: bool,
}

impl StreamLead {
    /// The share to tap events into.
    pub(crate) fn share(&self) -> Arc<StreamShare> {
        Arc::clone(&self.share)
    }

    /// Publish the terminal outcome and release the claim. Called exactly
    /// once per lead, on every path the stream task can end on. A failure
    /// published before the stream committed also fails the joiners'
    /// first-byte wait with the same error.
    pub(crate) fn publish(mut self, outcome: StreamOutcome) {
        self.published = true;
        if let Err(error) = outcome.clone() {
            self.share.fail_commit_if_pending(&error);
        }
        let _ = self.share.tx.send_replace(Some(outcome));
        self.registry.release(&self.identity, &self.share);
    }
}

impl Drop for StreamLead {
    fn drop(&mut self) {
        if !self.published {
            // The leader died unpublished: fail the joiners' first-byte wait
            // with the sentinel they re-run on, rather than leaving them
            // waiting forever.
            let error = EngineError::Internal(STREAM_LEADER_LOST.to_owned());
            self.share.fail_commit_if_pending(&error);
            let _ = self.share.tx.send_replace(Some(Err(error)));
            self.registry.release(&self.identity, &self.share);
        }
    }
}

/// Per-caller single-flight for streaming requests (plan 06 step 2): the
/// first identical concurrent request runs the upstream call and its events
/// are tapped into a share; the rest replay the collected events once the
/// leader's stream ends.
#[derive(Default)]
pub struct StreamShares {
    shares: Mutex<HashMap<CacheIdentity, Arc<StreamShare>>>,
}

impl std::fmt::Debug for StreamShares {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The count, never the events: responses must not reach logs.
        let len = self.shares.lock().map_or(0, |shares| shares.len());
        f.debug_struct("StreamShares")
            .field("in_flight", &len)
            .finish()
    }
}

impl StreamShares {
    /// A fresh registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or join the share for `key`.
    pub(crate) fn acquire(self: &Arc<Self>, key: &CacheKey<'_>) -> StreamAcquire {
        let identity = CacheIdentity::from_key(key);
        let mut shares = self.shares.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(share) = shares.get(&identity) {
            return StreamAcquire::Join(Arc::clone(share));
        }
        let (tx, _rx) = watch::channel(None);
        let (commit, _commit_rx) = watch::channel(None);
        let share = Arc::new(StreamShare {
            events: Mutex::new(Vec::new()),
            commit,
            tx,
        });
        shares.insert(identity.clone(), Arc::clone(&share));
        StreamAcquire::Lead(StreamLead {
            registry: Arc::clone(self),
            identity,
            share,
            published: false,
        })
    }

    fn release(&self, identity: &CacheIdentity, share: &Arc<StreamShare>) {
        let mut shares = self.shares.lock().unwrap_or_else(|e| e.into_inner());
        if shares
            .get(identity)
            .is_some_and(|registered| Arc::ptr_eq(registered, share))
        {
            shares.remove(identity);
        }
    }
}

/// A stored outcome for `Idempotency-Key` replay: the completed response of
/// a non-streaming request, or the collected events of a streaming one.
#[derive(Debug, Clone)]
pub struct StoredOutcome {
    /// The completed non-streaming response, when the first request was
    /// non-streaming.
    pub response: Option<EngineResponse>,
    /// The collected events, when the first request streamed.
    pub events: Vec<CoreEvent>,
}

/// What a claimed key is completed under, and what a retry must match: the
/// body digest plus the provider route that ran the first request. A key
/// reused against a different route with identical body bytes would else
/// replay the first provider's response under the second provider's name.
#[derive(Debug, Clone)]
pub(crate) struct IdemMatch {
    provider: String,
    route_kind: ProviderRouteKind,
    body_hash: String,
}

impl IdemMatch {
    pub(crate) fn new(provider: &str, route_kind: ProviderRouteKind, body_hash: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            route_kind,
            body_hash: body_hash.to_owned(),
        }
    }
}

/// What claiming an idempotency key returned.
pub(crate) enum IdempotencyAcquire {
    /// Nobody holds this key: this request claims it and must complete or
    /// release it exactly once.
    Claim(IdempotencyClaim),
    /// The first request with this key is still in flight: 409 (only while
    /// in flight; a retry after completion replays).
    InFlight,
    /// A completed outcome exists for this key, body and route: replay it.
    Replay(Box<StoredOutcome>),
    /// A completed outcome exists for this key under a different body or
    /// route: the client is reusing a key with different parameters.
    BodyMismatch,
}

/// The leader's claim on one idempotency key.
#[derive(Debug)]
pub struct IdempotencyClaim {
    store: Arc<IdempotencyStore>,
    key: IdemKey,
    matched: IdemMatch,
    completed: bool,
}

impl IdempotencyClaim {
    /// Store the successful outcome for replay until the TTL expires. Called
    /// exactly once; a failed request simply drops the claim, which releases
    /// the key so a retry executes again.
    pub(crate) fn complete(mut self, outcome: StoredOutcome) {
        self.completed = true;
        self.store.complete(&self.key, &self.matched, outcome);
    }
}

impl Drop for IdempotencyClaim {
    fn drop(&mut self) {
        if !self.completed {
            self.store.release(&self.key);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IdemKey {
    caller: String,
    idempotency_key: String,
}

enum IdemEntry {
    InFlight,
    Completed {
        matched: IdemMatch,
        outcome: Box<StoredOutcome>,
        completed_at: Instant,
        /// Insertion generation, so a re-completed key's older queue slot
        /// cannot evict or replay the newer entry.
        generation: u64,
    },
}

#[derive(Default)]
struct IdemTable {
    entries: HashMap<IdemKey, IdemEntry>,
    /// Completed entries in completion order with each insert's generation,
    /// for the TTL sweep and FIFO eviction at the capacity bound. In-flight
    /// claims are never queued: they end with their request.
    order: VecDeque<(IdemKey, u64)>,
    next_generation: u64,
}

/// The in-process `Idempotency-Key` store (plan 06 step 3, backlog 24):
/// claims a key before budget checks, replays the stored response for a
/// matching retry within the TTL, and reports in-flight conflicts (409) only
/// while the first request runs. Keys are caller-scoped. Failures are not
/// stored: a retry after a failed first attempt executes again. Completed
/// outcomes are swept once past the TTL and bounded FIFO at `max_entries`,
/// mirroring [`MemoryCache`].
pub struct IdempotencyStore {
    table: Mutex<IdemTable>,
    ttl: Duration,
    max_entries: usize,
}

impl std::fmt::Debug for IdempotencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keys and outcomes are caller data; log only the shape.
        f.debug_struct("IdempotencyStore")
            .field("entries", &self.table.lock().map_or(0, |t| t.entries.len()))
            .field("ttl", &self.ttl)
            .field("max_entries", &self.max_entries)
            .finish()
    }
}

impl IdempotencyStore {
    /// A store replaying outcomes for `ttl`, holding at most `max_entries`
    /// completed outcomes (the oldest completed is evicted first).
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            table: Mutex::new(IdemTable::default()),
            ttl,
            max_entries,
        }
    }

    pub(crate) fn claim(
        self: &Arc<Self>,
        caller: &str,
        idempotency_key: &str,
        matched: &IdemMatch,
    ) -> IdempotencyAcquire {
        let key = IdemKey {
            caller: caller.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
        };
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep_expired(&mut table);
        match table.entries.get_mut(&key) {
            Some(IdemEntry::InFlight) => IdempotencyAcquire::InFlight,
            Some(IdemEntry::Completed {
                matched: stored,
                outcome,
                completed_at,
                ..
            }) => {
                if completed_at.elapsed() >= self.ttl {
                    // The replay window passed: the key is claimable again.
                    table.entries.insert(key.clone(), IdemEntry::InFlight);
                    return IdempotencyAcquire::Claim(IdempotencyClaim {
                        store: Arc::clone(self),
                        key,
                        matched: matched.clone(),
                        completed: false,
                    });
                }
                if stored.body_hash == matched.body_hash
                    && stored.provider == matched.provider
                    && stored.route_kind == matched.route_kind
                {
                    IdempotencyAcquire::Replay(outcome.clone())
                } else {
                    IdempotencyAcquire::BodyMismatch
                }
            }
            None => {
                table.entries.insert(key.clone(), IdemEntry::InFlight);
                IdempotencyAcquire::Claim(IdempotencyClaim {
                    store: Arc::clone(self),
                    key,
                    matched: matched.clone(),
                    completed: false,
                })
            }
        }
    }

    fn complete(&self, key: &IdemKey, matched: &IdemMatch, outcome: StoredOutcome) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let generation = table.next_generation;
        table.next_generation += 1;
        table.entries.insert(
            key.clone(),
            IdemEntry::Completed {
                matched: matched.clone(),
                outcome: Box::new(outcome),
                completed_at: Instant::now(),
                generation,
            },
        );
        table.order.push_back((key.clone(), generation));
        self.sweep_expired(&mut table);
        self.enforce_bound(&mut table);
    }

    fn release(&self, key: &IdemKey) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.entries.remove(key);
    }

    /// Drops completed entries past the TTL. Completed entries age in queue
    /// order, so expired ones sit at the front; a superseded slot (its key
    /// was re-completed or reclaimed in flight) drops without its entry.
    fn sweep_expired(&self, table: &mut IdemTable) {
        while let Some((key, generation)) = table.order.front().cloned() {
            match table.entries.get(&key) {
                Some(IdemEntry::Completed {
                    completed_at,
                    generation: current,
                    ..
                }) if *current == generation => {
                    if completed_at.elapsed() < self.ttl {
                        break;
                    }
                    table.entries.remove(&key);
                    table.order.pop_front();
                }
                _ => {
                    table.order.pop_front();
                }
            }
        }
    }

    /// Evicts the oldest completed entries until at most `max_entries`
    /// remain. In-flight claims are never counted or evicted.
    fn enforce_bound(&self, table: &mut IdemTable) {
        let mut completed = table.order.len();
        while completed > self.max_entries {
            let Some((key, generation)) = table.order.pop_front() else {
                break;
            };
            if let Some(IdemEntry::Completed {
                generation: current,
                ..
            }) = table.entries.get(&key)
                && *current == generation
            {
                table.entries.remove(&key);
                completed -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_proxy_protocol::core::{CoreRequest, CoreResponse};

    fn plain_request() -> CoreRequest {
        CoreRequest {
            model: llm_proxy_protocol::core::ModelRef {
                requested: "m".to_owned(),
                upstream: None,
            },
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            sampling: Default::default(),
            stream: false,
            metadata: Default::default(),
            provider_hints: Default::default(),
        }
    }

    fn cache_key<'a>(body_hash: &'a str, request: &'a CoreRequest) -> CacheKey<'a> {
        CacheKey {
            caller: "caller-a",
            provider: "prov",
            route_kind: ProviderRouteKind::Messages,
            model: "m",
            body_hash,
            streaming: false,
            request,
        }
    }

    fn empty_entry() -> CacheEntry {
        CacheEntry {
            response: CoreResponse {
                id: None,
                model: llm_proxy_protocol::core::ModelRef {
                    requested: "m".to_owned(),
                    upstream: None,
                },
                content: Vec::new(),
                stop_reason: llm_proxy_protocol::core::StopReason::EndTurn,
                stop_sequence: None,
                usage: Default::default(),
                provider_meta: serde_json::Map::new(),
                cost: None,
            },
            events: Vec::new(),
        }
    }

    #[tokio::test]
    async fn expired_entries_do_not_leave_dead_queue_slots_behind() {
        // Distinct bodies churn the cache below its capacity bound and age
        // past the TTL: the queue must be reclaimed with the entries, not
        // accumulate one dead slot per expired identity (an unbounded leak
        // in the long-running daemon).
        let cache = MemoryCache::new(Duration::from_millis(30), 64);
        let request = plain_request();
        for i in 0..32 {
            let body_hash = format!("hash-{i}");
            cache
                .store(&cache_key(&body_hash, &request), &empty_entry())
                .await;
        }
        std::thread::sleep(Duration::from_millis(60));
        let hit = cache.lookup(&cache_key("hash-0", &request)).await;
        assert!(matches!(hit, Ok(None)), "every stored entry is expired");
        let table = cache.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            table.order.len(),
            table.entries.len(),
            "every queue slot must map to a live entry after expiry churn"
        );
        assert_eq!(table.entries.len(), 0, "nothing live remains");
    }

    fn matched(provider: &str, route_kind: ProviderRouteKind, body: &str) -> IdemMatch {
        IdemMatch {
            provider: provider.to_owned(),
            route_kind,
            body_hash: body.to_owned(),
        }
    }

    fn claim_key(
        store: &Arc<IdempotencyStore>,
        key: &str,
        matched: &IdemMatch,
    ) -> IdempotencyAcquire {
        store.claim("caller-a", key, matched)
    }

    fn complete_key(store: &Arc<IdempotencyStore>, key: &str, matched: &IdemMatch) {
        let IdempotencyAcquire::Claim(claim) = claim_key(store, key, matched) else {
            panic!("key {key} must be claimable");
        };
        claim.complete(StoredOutcome {
            response: None,
            events: Vec::new(),
        });
    }

    #[test]
    fn completed_outcomes_expire_and_release_their_key() {
        // A zero TTL means every completed outcome is past its replay window
        // the moment it lands: the sweep must drop it, not retain it for
        // reclaim-by-same-key only.
        let store = Arc::new(IdempotencyStore::new(Duration::ZERO, 16));
        complete_key(
            &store,
            "key-1",
            &matched("prov", ProviderRouteKind::Messages, "body"),
        );
        assert!(
            format!("{store:?}").contains("entries: 0"),
            "an expired outcome must be swept, not held: {store:?}"
        );
        assert!(
            matches!(
                claim_key(
                    &store,
                    "key-1",
                    &matched("prov", ProviderRouteKind::Messages, "body")
                ),
                IdempotencyAcquire::Claim(_)
            ),
            "the swept key is claimable again by any caller of claim"
        );
    }

    #[test]
    fn a_recompleted_expired_key_does_not_over_evict_at_the_bound() {
        // The one sequence that leaves a superseded queue slot: key-a
        // completes, key-b completes later, key-a expires while key-b is
        // fresh, key-a is re-claimed and re-completed. The bound (2) must
        // evict nothing here: the superseded slot is dropped by the TTL
        // sweep that runs before the bound, and both live outcomes replay.
        // If the bound ever charged superseded slots against the live count,
        // key-b (the oldest live outcome) would be evicted and re-execute.
        let store = Arc::new(IdempotencyStore::new(Duration::from_millis(500), 2));
        let messages = ProviderRouteKind::Messages;
        complete_key(&store, "key-a", &matched("prov", messages, "body-a"));
        std::thread::sleep(Duration::from_millis(300));
        complete_key(&store, "key-b", &matched("prov", messages, "body-b"));
        std::thread::sleep(Duration::from_millis(300));
        // key-a is past its 500ms TTL; key-b completed 300ms ago and is live.
        complete_key(&store, "key-a", &matched("prov", messages, "body-a"));

        assert!(
            matches!(
                claim_key(&store, "key-a", &matched("prov", messages, "body-a")),
                IdempotencyAcquire::Replay(_)
            ),
            "the re-completed outcome replays"
        );
        assert!(
            matches!(
                claim_key(&store, "key-b", &matched("prov", messages, "body-b")),
                IdempotencyAcquire::Replay(_)
            ),
            "the oldest live outcome must not be over-evicted by a superseded slot"
        );

        // The bound still evicts exactly the oldest live outcome when a third
        // completes.
        complete_key(&store, "key-c", &matched("prov", messages, "body-c"));
        assert!(
            matches!(
                claim_key(&store, "key-b", &matched("prov", messages, "body-b")),
                IdempotencyAcquire::Claim(_)
            ),
            "key-b is the oldest live outcome once the bound is exceeded"
        );
        assert!(
            matches!(
                claim_key(&store, "key-a", &matched("prov", messages, "body-a")),
                IdempotencyAcquire::Replay(_)
            ),
            "the newest live outcome stays within the bound"
        );
    }

    #[test]
    fn the_bound_evicts_the_oldest_completed_outcome() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 1));
        let messages = ProviderRouteKind::Messages;
        complete_key(&store, "key-1", &matched("prov", messages, "body-1"));
        complete_key(&store, "key-2", &matched("prov", messages, "body-2"));

        assert!(
            matches!(
                claim_key(&store, "key-2", &matched("prov", messages, "body-2")),
                IdempotencyAcquire::Replay(_)
            ),
            "the newest outcome stays within the bound"
        );
        assert!(
            matches!(
                claim_key(&store, "key-1", &matched("prov", messages, "body-1")),
                IdempotencyAcquire::Claim(_)
            ),
            "the oldest outcome was evicted, so its key executes again"
        );
    }

    #[test]
    fn a_released_in_flight_key_is_never_swept_or_counted() {
        // A dropped (failed) claim releases its key; the sweep and the bound
        // must not resurrect or evict around in-flight state.
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 2));
        let IdempotencyAcquire::Claim(claim) = claim_key(
            &store,
            "key-1",
            &matched("prov", ProviderRouteKind::Messages, "body"),
        ) else {
            panic!("key must be claimable");
        };
        drop(claim);
        assert!(
            matches!(
                claim_key(
                    &store,
                    "key-1",
                    &matched("prov", ProviderRouteKind::Messages, "body")
                ),
                IdempotencyAcquire::Claim(_)
            ),
            "a released key is claimable again immediately"
        );
    }

    #[test]
    fn a_key_completed_under_one_route_does_not_replay_for_another() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 16));
        let messages = ProviderRouteKind::Messages;
        complete_key(&store, "key-1", &matched("prov-a", messages, "same-body"));

        assert!(
            matches!(
                claim_key(&store, "key-1", &matched("prov-a", messages, "same-body")),
                IdempotencyAcquire::Replay(_)
            ),
            "same provider, route and body replays"
        );
        assert!(
            matches!(
                claim_key(&store, "key-1", &matched("prov-b", messages, "same-body")),
                IdempotencyAcquire::BodyMismatch
            ),
            "the same body bytes under another provider must not replay"
        );
        assert!(
            matches!(
                claim_key(
                    &store,
                    "key-1",
                    &matched("prov-a", ProviderRouteKind::ChatCompletions, "same-body")
                ),
                IdempotencyAcquire::BodyMismatch
            ),
            "the same body bytes under another route kind must not replay"
        );
    }
}
