//! Usage events into a Redis stream (plan 09 step 1): every replica
//! appends to the same stream, so one reader drains the fleet's ledger
//! feed. This is the shared-store counterpart of the SQLite ledger: the
//! events are the same [`ProxyEvent`]s; storage is the operator's to
//! consume (a drain job, a projector into Postgres, an exporter).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use llm_proxy_storage::{EventBus, ProxyEvent};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use tokio::sync::mpsc;

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// [`EventBus`] appending every usage event to one Redis stream.
/// `emit` never blocks — a full channel drops the event and counts it in
/// [`RedisEventBus::dropped_events`]. Open inside a tokio runtime.
#[derive(Debug)]
pub struct RedisEventBus {
    tx: mpsc::Sender<ProxyEvent>,
    dropped: Arc<AtomicU64>,
    stream_key: String,
}

impl RedisEventBus {
    /// Append to `stream_key` on the store's Redis. Call inside a tokio
    /// runtime (the writer task is spawned here).
    #[must_use]
    pub fn new(store: crate::RedisStore, stream_key: &str) -> Self {
        Self::with_capacity(store, stream_key, DEFAULT_CHANNEL_CAPACITY)
    }

    /// Same, with an explicit channel capacity (tests use a small one).
    #[must_use]
    pub fn with_capacity(store: crate::RedisStore, stream_key: &str, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let stream_key = if stream_key.is_empty() {
            format!("{}:usage", store.prefix)
        } else {
            stream_key.to_owned()
        };
        let bus = Self {
            tx,
            dropped,
            stream_key,
        };
        tokio::spawn(writer_task(
            store.conn,
            rx,
            bus.stream_key.clone(),
            Arc::clone(&bus.dropped),
        ));
        bus
    }

    /// Number of events dropped so far (channel full, or writer gone).
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The stream events are appended to.
    #[must_use]
    pub fn stream_key(&self) -> &str {
        &self.stream_key
    }
}

async fn writer_task(
    mut conn: ConnectionManager,
    mut rx: mpsc::Receiver<ProxyEvent>,
    stream_key: String,
    dropped: Arc<AtomicU64>,
) {
    while let Some(event) = rx.recv().await {
        if let Err(error) = append(&mut conn, &stream_key, &event, &dropped).await {
            let lost = dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                %error,
                lost,
                "appending a usage event to the Redis stream failed; event dropped"
            );
        }
    }
}

async fn append(
    conn: &mut ConnectionManager,
    stream_key: &str,
    event: &ProxyEvent,
    dropped: &Arc<AtomicU64>,
) -> Result<(), redis::RedisError> {
    let payload = match serde_json::to_string(event) {
        Ok(payload) => payload,
        Err(error) => {
            // A ProxyEvent always serializes; this is a programming error,
            // paid for by dropping the event, never by blocking the caller.
            let lost = dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(%error, lost, "serializing a usage event failed; dropped");
            return Ok(());
        }
    };
    let _: Option<String> = conn.xadd(stream_key, "*", &[("event", payload)]).await?;
    Ok(())
}

impl EventBus for RedisEventBus {
    fn emit(&self, event: &ProxyEvent) {
        match self.tx.try_send(event.clone()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    dropped,
                    "usage-stream channel full; event dropped (never backpressure a response)"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(dropped, "usage-stream writer is gone; event dropped");
            }
        }
    }

    fn close(&self) {
        // Drop our sender so the writer drains what is queued and exits;
        // Redis streams have no in-stream flush to wait for.
    }
}
