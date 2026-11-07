//! Usage events into a Postgres table (plan 09 step 1): every replica
//! appends to the same `usage_events` table, giving the fleet one durable
//! ledger feed. The events are the same [`ProxyEvent`]s the SQLite ledger
//! stores; the payload column holds the JSON.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use llm_proxy_storage::{EventBus, ProxyEvent};
use tokio::sync::mpsc;
const DEFAULT_CHANNEL_CAPACITY: usize = 1024;
const WRITE_BATCH_MAX: usize = 256;

/// [`EventBus`] appending every usage event to `usage_events` as one JSON
/// payload per row. `emit` never blocks — a full channel drops the event
/// and counts it in [`PostgresEventBus::dropped_events`]. Open inside a
/// tokio runtime.
#[derive(Debug)]
pub struct PostgresEventBus {
    tx: mpsc::Sender<ProxyEvent>,
    dropped: Arc<AtomicU64>,
}

impl PostgresEventBus {
    /// Over a connected store. Call inside a tokio runtime (the writer
    /// task is spawned here).
    #[must_use]
    pub fn new(store: crate::PostgresStore) -> Self {
        Self::with_capacity(store, DEFAULT_CHANNEL_CAPACITY)
    }

    /// Same, with an explicit channel capacity (tests use a small one).
    #[must_use]
    pub fn with_capacity(store: crate::PostgresStore, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        tokio::spawn(writer_task(store, rx, Arc::clone(&dropped)));
        Self { tx, dropped }
    }

    /// Number of events dropped so far (channel full, writer gone, or a
    /// failed batch).
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

async fn writer_task(
    store: crate::PostgresStore,
    mut rx: mpsc::Receiver<ProxyEvent>,
    dropped: Arc<AtomicU64>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(WRITE_BATCH_MAX);
        batch.push(first);
        while batch.len() < WRITE_BATCH_MAX
            && let Ok(event) = rx.try_recv()
        {
            batch.push(event);
        }
        let mut client = match store.connection().await {
            Ok(client) => client,
            Err(error) => {
                let lost =
                    dropped.fetch_add(batch.len() as u64, Ordering::Relaxed) + batch.len() as u64;
                tracing::warn!(%error, lost, "opening a Postgres connection for a usage batch failed");
                continue;
            }
        };
        if write_batch(&mut client, &batch).await.is_err() {
            let lost =
                dropped.fetch_add(batch.len() as u64, Ordering::Relaxed) + batch.len() as u64;
            tracing::warn!(
                lost,
                "appending a usage batch to Postgres failed; batch dropped"
            );
        }
    }
}

async fn write_batch(
    client: &mut tokio_postgres::Client,
    batch: &[ProxyEvent],
) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    for event in batch {
        let payload = match serde_json::to_value(event) {
            Ok(payload) => payload,
            Err(error) => {
                // A ProxyEvent always serializes; treat a failure as a
                // programming error and drop only this row.
                tracing::error!(%error, "serializing a usage event failed; row dropped");
                continue;
            }
        };
        tx.execute(
            "INSERT INTO usage_events (payload) VALUES ($1)",
            &[&payload],
        )
        .await?;
    }
    tx.commit().await
}

impl EventBus for PostgresEventBus {
    fn emit(&self, event: &ProxyEvent) {
        match self.tx.try_send(event.clone()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    dropped,
                    "usage-table channel full; event dropped (never backpressure a response)"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(dropped, "usage-table writer is gone; event dropped");
            }
        }
    }

    fn close(&self) {
        // Drop our sender so the writer drains the queued batch and exits.
    }
}
