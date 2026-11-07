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
}

#[async_trait::async_trait]
impl Cache for MemoryCache {
    async fn lookup(&self, key: &CacheKey<'_>) -> Result<Option<CacheEntry>, EngineError> {
        let identity = CacheIdentity::from_key(key);
        let mut table = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match table.entries.get(&identity) {
            Some((entry, stored_at, _)) if !self.expired(*stored_at) => Ok(Some(entry.clone())),
            Some(_) => {
                // Expired: drop it so the capacity bound counts live entries.
                table.entries.remove(&identity);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn store(&self, key: &CacheKey<'_>, entry: &CacheEntry) {
        let identity = CacheIdentity::from_key(key);
        let mut table = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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

impl Drop for FlightLead {
    fn drop(&mut self) {
        if !self.published {
            // The leader ended without publishing (a bug or a panic): fail
            // the followers rather than leaving them waiting forever.
            let _ = self.share.tx.send_replace(Some(Err(EngineError::Internal(
                "single-flight leader ended without an outcome".to_owned(),
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

/// One shared streaming request: the events the leader has forwarded so far
/// and the terminal outcome.
pub(crate) struct StreamShare {
    events: Mutex<Vec<CoreEvent>>,
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
    /// once per lead, on every path the stream task can end on.
    pub(crate) fn publish(mut self, outcome: StreamOutcome) {
        self.published = true;
        let _ = self.share.tx.send_replace(Some(outcome));
        self.registry.release(&self.identity, &self.share);
    }
}

impl Drop for StreamLead {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.share.tx.send_replace(Some(Err(EngineError::Internal(
                "shared stream leader ended without an outcome".to_owned(),
            ))));
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
        let share = Arc::new(StreamShare {
            events: Mutex::new(Vec::new()),
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

/// What claiming an idempotency key returned.
pub(crate) enum IdempotencyAcquire {
    /// Nobody holds this key: this request claims it and must complete or
    /// release it exactly once.
    Claim(IdempotencyClaim),
    /// The first request with this key is still in flight: 409 (only while
    /// in flight; a retry after completion replays).
    InFlight,
    /// A completed outcome exists for this key and body: replay it.
    Replay(Box<StoredOutcome>),
    /// A completed outcome exists for this key under a different body: the
    /// client is reusing a key with different parameters.
    BodyMismatch,
}

/// The leader's claim on one idempotency key.
#[derive(Debug)]
pub struct IdempotencyClaim {
    store: Arc<IdempotencyStore>,
    key: IdemKey,
    body_hash: String,
    completed: bool,
}

impl IdempotencyClaim {
    /// Store the successful outcome for replay until the TTL expires. Called
    /// exactly once; a failed request simply drops the claim, which releases
    /// the key so a retry executes again.
    pub(crate) fn complete(mut self, outcome: StoredOutcome) {
        self.completed = true;
        self.store.complete(&self.key, &self.body_hash, outcome);
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
        body_hash: String,
        outcome: Box<StoredOutcome>,
        completed_at: Instant,
    },
}

/// The in-process `Idempotency-Key` store (plan 06 step 3, backlog 24):
/// claims a key before budget checks, replays the stored response for a
/// matching retry within the TTL, and reports in-flight conflicts (409) only
/// while the first request runs. Keys are caller-scoped. Failures are not
/// stored: a retry after a failed first attempt executes again.
pub struct IdempotencyStore {
    entries: Mutex<HashMap<IdemKey, IdemEntry>>,
    ttl: Duration,
}

impl std::fmt::Debug for IdempotencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keys and outcomes are caller data; log only the shape.
        f.debug_struct("IdempotencyStore")
            .field("entries", &self.entries.lock().map_or(0, |e| e.len()))
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl IdempotencyStore {
    /// A store replaying outcomes for `ttl`.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub(crate) fn claim(
        self: &Arc<Self>,
        caller: &str,
        idempotency_key: &str,
        body_hash: &str,
    ) -> IdempotencyAcquire {
        let key = IdemKey {
            caller: caller.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get_mut(&key) {
            Some(IdemEntry::InFlight) => IdempotencyAcquire::InFlight,
            Some(IdemEntry::Completed {
                body_hash: stored_hash,
                outcome,
                completed_at,
            }) => {
                if completed_at.elapsed() >= self.ttl {
                    // The replay window passed: the key is claimable again.
                    entries.insert(key.clone(), IdemEntry::InFlight);
                    return IdempotencyAcquire::Claim(IdempotencyClaim {
                        store: Arc::clone(self),
                        key,
                        body_hash: body_hash.to_owned(),
                        completed: false,
                    });
                }
                if stored_hash == body_hash {
                    IdempotencyAcquire::Replay(outcome.clone())
                } else {
                    IdempotencyAcquire::BodyMismatch
                }
            }
            None => {
                entries.insert(key.clone(), IdemEntry::InFlight);
                IdempotencyAcquire::Claim(IdempotencyClaim {
                    store: Arc::clone(self),
                    key,
                    body_hash: body_hash.to_owned(),
                    completed: false,
                })
            }
        }
    }

    fn complete(&self, key: &IdemKey, body_hash: &str, outcome: StoredOutcome) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            key.clone(),
            IdemEntry::Completed {
                body_hash: body_hash.to_owned(),
                outcome: Box::new(outcome),
                completed_at: Instant::now(),
            },
        );
    }

    fn release(&self, key: &IdemKey) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.remove(key);
    }
}
