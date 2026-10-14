use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use llm_proxy_protocol::core::{StopReason, UsageProvenance};
use rusqlite::Connection;
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::watch;

use crate::{EventBus, ProxyEvent};

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;
const WRITE_BATCH_MAX: usize = 256;
const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from the SQLite usage ledger.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Failed to create the ledger file's parent directory.
    #[error("creating usage-ledger parent directory {path}: {source}")]
    CreateDir {
        /// Parent directory that could not be created.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Failed to open or initialize the SQLite database.
    #[error("opening usage ledger at {path}: {source}")]
    Open {
        /// Database path.
        path: String,
        /// Underlying rusqlite error.
        source: rusqlite::Error,
    },
    /// A write or retention operation on an open ledger failed.
    #[error("usage ledger operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Retention bounds applied to the ledger on a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Delete rows older than this age. `None` keeps rows forever.
    pub max_age: Option<Duration>,
    /// Keep at most this many rows; the oldest rows are deleted when the cap
    /// is exceeded. `None` is unlimited.
    pub max_rows: Option<u64>,
    /// How often the pruning pass runs; must be at least 1 second (a zero
    /// period is rejected by the pruning timer).
    pub prune_every: Duration,
}

impl RetentionPolicy {
    fn is_bounded(&self) -> bool {
        self.max_age.is_some() || self.max_rows.is_some()
    }
}

/// Run one retention pass, deleting rows past `policy`'s bounds; returns the
/// number of rows deleted. Age pruning covers failure-only rows (no
/// `received_at`) via `COALESCE(received_at, completed_at)`.
pub fn run_retention_pass(
    conn: &mut Connection,
    policy: &RetentionPolicy,
) -> Result<u64, StorageError> {
    if !policy.is_bounded() {
        return Ok(0);
    }
    let mut deleted: u64 = 0;
    if let Some(max_age) = policy.max_age {
        let cutoff = unix_millis(OffsetDateTime::now_utc())
            .saturating_sub(i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX));
        let n = conn.execute(
            "DELETE FROM requests WHERE COALESCE(received_at, completed_at) < ?1",
            rusqlite::params![cutoff],
        )?;
        deleted += u64::try_from(n).unwrap_or(u64::MAX);
    }
    if let Some(max_rows) = policy.max_rows {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?;
        let excess = u64::try_from(count)
            .unwrap_or(u64::MAX)
            .saturating_sub(max_rows);
        if excess > 0 {
            let limit = i64::try_from(excess).unwrap_or(i64::MAX);
            let n = conn.execute(
                "DELETE FROM requests WHERE request_id IN (
                     SELECT request_id FROM requests
                     ORDER BY COALESCE(received_at, completed_at) ASC
                     LIMIT ?1
                 )",
                rusqlite::params![limit],
            )?;
            deleted += u64::try_from(n).unwrap_or(u64::MAX);
        }
    }
    Ok(deleted)
}

const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS requests (
    request_id            TEXT PRIMARY KEY,
    received_at           INTEGER,
    completed_at          INTEGER,
    caller                TEXT,
    provider              TEXT,
    route_kind            TEXT,
    client_protocol       TEXT,
    streaming             INTEGER,
    model_requested       TEXT,
    model_upstream        TEXT,
    input_tokens          INTEGER,
    output_tokens         INTEGER,
    reasoning_tokens      INTEGER,
    cache_read_tokens     INTEGER,
    cache_creation_tokens INTEGER,
    usage_provenance      TEXT,
    cost_input            TEXT,
    cost_output           TEXT,
    cost_cache_creation   TEXT,
    cost_cache_read       TEXT,
    cost_reasoning        TEXT,
    cost_total            TEXT,
    stop_reason           TEXT,
    error_kind            TEXT,
    http_status           INTEGER,
    latency_ms            INTEGER,
    body_hash             TEXT
);
CREATE INDEX IF NOT EXISTS idx_requests_timestamp
    ON requests (received_at);
CREATE INDEX IF NOT EXISTS idx_requests_provider_timestamp
    ON requests (provider, received_at);
CREATE INDEX IF NOT EXISTS idx_requests_caller_timestamp
    ON requests (caller, received_at);
";

fn open_connection(path: &Path) -> Result<Connection, StorageError> {
    let open_err = |source: rusqlite::Error| StorageError::Open {
        path: path.display().to_string(),
        source,
    };
    let conn = Connection::open(path).map_err(open_err)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(open_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(open_err)?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(open_err)?;
    conn.execute_batch(SCHEMA_SQL).map_err(open_err)?;
    Ok(conn)
}

fn unix_millis(ts: OffsetDateTime) -> i64 {
    ts.unix_timestamp()
        .saturating_mul(1000)
        .saturating_add(i64::from(ts.millisecond()))
}

fn stop_reason_label(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::MaxTokens => "max_tokens".to_owned(),
        StopReason::ToolUse => "tool_use".to_owned(),
        StopReason::StopSequence => "stop_sequence".to_owned(),
        StopReason::Refusal => "refusal".to_owned(),
        StopReason::Error => "error".to_owned(),
        StopReason::Unknown(inner) => format!("unknown:{inner}"),
        _ => "other".to_owned(),
    }
}

fn provenance_label(provenance: &UsageProvenance) -> &'static str {
    match provenance {
        UsageProvenance::ProviderReported => "provider_reported",
        UsageProvenance::SyntheticZero => "synthetic_zero",
        UsageProvenance::Unknown => "unknown",
        _ => "other",
    }
}

fn write_event(conn: &Connection, event: &ProxyEvent) -> Result<(), StorageError> {
    match event {
        ProxyEvent::RequestReceived(e) => insert_request_received(conn, e),
        ProxyEvent::ResponseCompleted(e) => upsert_response_completed(conn, e),
        ProxyEvent::ResponseFailed(e) => upsert_response_failed(conn, e),
    }
}

fn insert_request_received(
    conn: &Connection,
    e: &crate::RequestReceived,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR IGNORE INTO requests (
                     request_id, received_at, caller, provider, route_kind,
                     client_protocol, streaming, model_requested, model_upstream,
                     body_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            e.request_id,
            unix_millis(e.timestamp),
            e.caller,
            e.provider,
            e.route_kind,
            e.client_protocol,
            e.streaming,
            e.model.requested,
            e.model.upstream,
            e.body_hash,
        ],
    )?;
    Ok(())
}

fn upsert_response_completed(
    conn: &Connection,
    e: &crate::ResponseCompleted,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO requests (
                     request_id, completed_at, provider, model_requested,
                     model_upstream, input_tokens, output_tokens, reasoning_tokens,
                     cache_read_tokens, cache_creation_tokens, usage_provenance,
                     cost_input, cost_output, cost_cache_creation, cost_cache_read,
                     cost_reasoning, cost_total, stop_reason, latency_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                           ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(request_id) DO UPDATE SET
                     completed_at = excluded.completed_at,
                     provider = excluded.provider,
                     model_requested = excluded.model_requested,
                     model_upstream = excluded.model_upstream,
                     input_tokens = excluded.input_tokens,
                     output_tokens = excluded.output_tokens,
                     reasoning_tokens = excluded.reasoning_tokens,
                     cache_read_tokens = excluded.cache_read_tokens,
                     cache_creation_tokens = excluded.cache_creation_tokens,
                     usage_provenance = excluded.usage_provenance,
                     cost_input = excluded.cost_input,
                     cost_output = excluded.cost_output,
                     cost_cache_creation = excluded.cost_cache_creation,
                     cost_cache_read = excluded.cost_cache_read,
                     cost_reasoning = excluded.cost_reasoning,
                     cost_total = excluded.cost_total,
                     stop_reason = excluded.stop_reason,
                     latency_ms = excluded.latency_ms",
        rusqlite::params![
            e.request_id,
            unix_millis(e.timestamp),
            e.provider,
            e.model.requested,
            e.model.upstream,
            e.usage.input_tokens,
            e.usage.output_tokens,
            e.usage.reasoning_tokens,
            e.usage.cache_read_input_tokens,
            e.usage.cache_creation_input_tokens,
            provenance_label(&e.usage.provenance),
            e.cost.as_ref().map(|c| c.input.to_string()),
            e.cost.as_ref().map(|c| c.output.to_string()),
            e.cost.as_ref().map(|c| c.cache_creation.to_string()),
            e.cost.as_ref().map(|c| c.cache_read.to_string()),
            e.cost.as_ref().map(|c| c.reasoning.to_string()),
            e.cost.as_ref().map(|c| c.total.to_string()),
            stop_reason_label(&e.stop_reason),
            i64::try_from(e.latency_ms).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

fn upsert_response_failed(
    conn: &Connection,
    e: &crate::ResponseFailed,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO requests (
                     request_id, completed_at, provider, model_requested,
                     model_upstream, error_kind, http_status, latency_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(request_id) DO UPDATE SET
                     completed_at = excluded.completed_at,
                     provider = COALESCE(excluded.provider, requests.provider),
                     model_requested = COALESCE(
                         excluded.model_requested, requests.model_requested),
                     model_upstream = COALESCE(
                         excluded.model_upstream, requests.model_upstream),
                     error_kind = excluded.error_kind,
                     http_status = excluded.http_status,
                     latency_ms = excluded.latency_ms",
        rusqlite::params![
            e.request_id,
            unix_millis(e.timestamp),
            e.provider,
            e.model.as_ref().map(|m| m.requested.clone()),
            e.model.as_ref().and_then(|m| m.upstream.clone()),
            e.error_kind,
            i64::from(e.http_status),
            i64::try_from(e.latency_ms).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

enum WriterOp {
    Write { events: Vec<ProxyEvent> },
    Prune { policy: RetentionPolicy },
}

fn run_op(conn: &mut Connection, op: WriterOp) -> Result<u64, StorageError> {
    match op {
        WriterOp::Write { events } => {
            let tx = conn.transaction()?;
            for event in &events {
                write_event(&tx, event)?;
            }
            tx.commit()?;
            Ok(u64::try_from(events.len()).unwrap_or(u64::MAX))
        }
        WriterOp::Prune { policy } => run_retention_pass(conn, &policy),
    }
}

async fn exec_on_conn(
    path: &Path,
    conn: Connection,
    op: WriterOp,
) -> Option<(Connection, Result<u64, StorageError>)> {
    let path = path.to_path_buf();
    let joined = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        let result = run_op(&mut conn, op);
        (conn, result)
    })
    .await;
    match joined {
        Ok((conn, result)) => Some((conn, result)),
        Err(join_error) => {
            tracing::error!(
                error = %join_error,
                "usage-ledger writer panicked; reopening the database"
            );
            let reopen_path = path.clone();
            let reopened = tokio::task::spawn_blocking(move || open_connection(&reopen_path)).await;
            match reopened {
                Ok(Ok(conn)) => Some((conn, Ok(0))),
                Ok(Err(error)) => {
                    tracing::error!(
                        error = %error,
                        path = %path.display(),
                        "failed to reopen usage ledger; writer stopping"
                    );
                    None
                }
                Err(join_error) => {
                    tracing::error!(
                        error = %join_error,
                        "reopening the usage ledger panicked; writer stopping"
                    );
                    None
                }
            }
        }
    }
}

type DoneFlag = Arc<(Mutex<bool>, Condvar)>;

fn signal_done(done: &DoneFlag) {
    let mut guard = done.0.lock().unwrap_or_else(|e| e.into_inner());
    *guard = true;
    drop(guard);
    done.1.notify_all();
}

async fn writer_task(
    path: PathBuf,
    conn: Connection,
    mut rx: mpsc::Receiver<ProxyEvent>,
    mut shutdown: watch::Receiver<bool>,
    retention: RetentionPolicy,
    done: DoneFlag,
) {
    let pruning = retention.is_bounded();
    let mut prune = tokio::time::interval(retention.prune_every.max(Duration::from_secs(1)));
    prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut conn_slot = Some(conn);
    let mut buf: Vec<ProxyEvent> = Vec::with_capacity(WRITE_BATCH_MAX);

    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            received = rx.recv_many(&mut buf, WRITE_BATCH_MAX) => {
                if received == 0 {
                    break;
                }
                let Some(conn) = conn_slot.take() else { break };
                match exec_on_conn(
                    &path,
                    conn,
                    WriterOp::Write { events: std::mem::take(&mut buf) },
                )
                .await
                {
                    Some((c, result)) => {
                        conn_slot = Some(c);
                        if let Err(error) = result {
                            tracing::error!(
                                error = %error,
                                "usage-ledger batch write failed; batch events lost"
                            );
                        }
                    }
                    None => break,
                }
            }
            _ = prune.tick(), if pruning => {
                if let Some(conn) = conn_slot.take() {
                    match exec_on_conn(
                        &path,
                        conn,
                        WriterOp::Prune { policy: retention },
                    )
                    .await
                    {
                        Some((c, result)) => {
                            conn_slot = Some(c);
                            match result {
                                Ok(rows) if rows > 0 => tracing::info!(
                                    rows,
                                    "pruned usage-ledger rows past retention bounds"
                                ),
                                Ok(_) => {}
                                Err(error) => tracing::error!(
                                    error = %error,
                                    "usage-ledger retention pass failed"
                                ),
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    while let Ok(event) = rx.try_recv() {
        buf.push(event);
    }
    if !buf.is_empty() {
        if let Some(conn) = conn_slot.take() {
            match exec_on_conn(&path, conn, WriterOp::Write { events: buf }).await {
                Some((_conn, Ok(_))) => {}
                Some((_conn, Err(error))) => {
                    tracing::error!(error = %error, "usage-ledger final drain failed");
                }
                None => {}
            }
        }
    }

    signal_done(&done);
}

/// SQLite-backed [`EventBus`]: persists every event to one `requests` ledger.
/// `emit` never blocks — a full channel drops the event and increments
/// [`SqliteEventBus::dropped_events`]. Open inside a tokio runtime.
#[derive(Debug)]
pub struct SqliteEventBus {
    tx: Sender<ProxyEvent>,
    shutdown_tx: Mutex<watch::Sender<bool>>,
    done: DoneFlag,
    dropped: AtomicU64,
    path: PathBuf,
}

impl SqliteEventBus {
    /// Open (or create) the ledger at `path` and start its writer task (parent
    /// directories, WAL mode, and schema created as needed). Blocking
    /// filesystem work: call at startup, not on the hot path.
    pub fn open(path: impl AsRef<Path>, retention: RetentionPolicy) -> Result<Self, StorageError> {
        Self::open_with_capacity(path, retention, DEFAULT_CHANNEL_CAPACITY)
    }

    fn open_with_capacity(
        path: impl AsRef<Path>,
        retention: RetentionPolicy,
        capacity: usize,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| StorageError::CreateDir {
                    path: parent.display().to_string(),
                    source,
                })?;
            }
        }
        let conn = open_connection(path)?;
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let done: DoneFlag = Arc::new((Mutex::new(false), Condvar::new()));
        tokio::spawn(writer_task(
            path.to_path_buf(),
            conn,
            rx,
            shutdown_rx,
            retention,
            Arc::clone(&done),
        ));
        Ok(Self {
            tx,
            shutdown_tx: Mutex::new(shutdown_tx),
            done,
            dropped: AtomicU64::new(0),
            path: path.to_path_buf(),
        })
    }

    #[cfg(test)]
    fn unconnected(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(rx));
        Self {
            tx,
            shutdown_tx: Mutex::new(shutdown_tx),
            done: Arc::new((Mutex::new(false), Condvar::new())),
            dropped: AtomicU64::new(0),
            path: PathBuf::new(),
        }
    }

    /// Number of events dropped so far (channel full, or writer gone).
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Path of the backing database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl EventBus for SqliteEventBus {
    fn emit(&self, event: &ProxyEvent) {
        match self.tx.try_send(event.clone()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    dropped,
                    "usage-ledger channel full; event dropped (never backpressure a response)"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(dropped, "usage-ledger writer is gone; event dropped");
            }
        }
    }

    fn close(&self) {
        if let Ok(shutdown_tx) = self.shutdown_tx.lock() {
            if shutdown_tx.send(true).is_err() {
                tracing::debug!("usage-ledger writer had already exited");
            }
        }
        let deadline = Instant::now() + CLOSE_DRAIN_TIMEOUT;
        let mut guard = self.done.0.lock().unwrap_or_else(|e| e.into_inner());
        while !*guard {
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(
                    timeout_secs = CLOSE_DRAIN_TIMEOUT.as_secs(),
                    "usage-ledger close timed out; queued events may not be flushed"
                );
                return;
            }
            let (next, timeout) = self
                .done
                .1
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if timeout.timed_out() {
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RequestReceived, ResponseCompleted, ResponseFailed};
    use llm_proxy_protocol::core::{Cost, ModelRef, Usage};
    use rust_decimal::Decimal;

    const RECEIVED_MS: i64 = 1_700_000_000_000;
    const COMPLETED_MS: i64 = 1_700_000_004_000;

    fn unbounded() -> RetentionPolicy {
        RetentionPolicy {
            max_age: None,
            max_rows: None,
            prune_every: Duration::from_secs(3600),
        }
    }

    fn received_event(id: &str, caller: &str) -> ProxyEvent {
        ProxyEvent::RequestReceived(RequestReceived {
            request_id: id.to_owned(),
            timestamp: OffsetDateTime::from_unix_timestamp(RECEIVED_MS / 1000)
                .expect("valid unix timestamp"),
            caller: caller.to_owned(),
            provider: "prov-a".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: ModelRef {
                requested: "claude-3".to_owned(),
                upstream: Some("claude-3-upstream".to_owned()),
            },
            streaming: true,
            body_hash: "sha256-digest-only".to_owned(),
        })
    }

    fn completed_event(id: &str) -> ProxyEvent {
        ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: id.to_owned(),
            timestamp: OffsetDateTime::from_unix_timestamp(COMPLETED_MS / 1000)
                .expect("valid unix timestamp"),
            provider: "prov-a".to_owned(),
            upstream_message_id: Some("msg_7".to_owned()),
            model: ModelRef {
                requested: "claude-3".to_owned(),
                upstream: None,
            },
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                reasoning_tokens: Some(7),
                cache_creation_input_tokens: Some(30),
                cache_read_input_tokens: Some(200),
                provenance: UsageProvenance::ProviderReported,
            },
            cost: Some(Cost {
                input: Decimal::from_str_exact("0.000123456789012345678901234")
                    .expect("decimal fits"),
                output: Decimal::from_str_exact("2.25").expect("decimal fits"),
                cache_creation: Decimal::ZERO,
                cache_read: Decimal::from_str_exact("0.0000005").expect("decimal fits"),
                reasoning: Decimal::ZERO,
                total: Decimal::from_str_exact("2.250123956789012345678901234")
                    .expect("decimal fits"),
            }),
            stop_reason: StopReason::Unknown("weird-finish".to_owned()),
            latency_ms: 4321,
        })
    }

    fn failed_event(id: &str, provider: Option<&str>) -> ProxyEvent {
        ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: id.to_owned(),
            timestamp: OffsetDateTime::from_unix_timestamp(COMPLETED_MS / 1000)
                .expect("valid unix timestamp"),
            provider: provider.map(str::to_owned),
            model: None,
            error_kind: "UpstreamError".to_owned(),
            message: "sanitized failure text".to_owned(),
            http_status: 502,
            latency_ms: 99,
        })
    }

    async fn wait_until(description: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within 2s: {description}");
    }

    fn reader(path: &Path) -> Connection {
        Connection::open(path).expect("open ledger for assertions")
    }

    fn scalar(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("scalar query")
    }

    fn text_column(conn: &Connection, sql: &str) -> Option<String> {
        conn.query_row(sql, [], |row| row.get::<_, Option<String>>(0))
            .expect("text query")
    }

    #[tokio::test]
    async fn open_enables_wal_and_creates_schema_and_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");
        let bus = Arc::new(bus);
        let closer = Arc::clone(&bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");

        let conn = reader(&db);
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        assert_eq!(mode, "wal", "ledger must run in WAL mode");

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")
                .expect("prepare index query");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query indexes");
            let mut names = Vec::new();
            for row in rows {
                names.push(row.expect("index row"));
            }
            names
        };
        for expected in [
            "idx_requests_timestamp",
            "idx_requests_provider_timestamp",
            "idx_requests_caller_timestamp",
        ] {
            assert!(
                indexes.contains(&expected.to_owned()),
                "missing index {expected}; got {indexes:?}"
            );
        }
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM requests"),
            0,
            "a fresh ledger starts empty"
        );
    }

    #[tokio::test]
    async fn request_received_persists_row_with_caller_and_body_hash() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");
        bus.emit(&received_event("req-1", "laptop"));
        let conn = reader(&db);
        wait_until("request row appears", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests WHERE request_id = 'req-1'",
            ) == 1
        })
        .await;

        let row = conn
            .query_row(
                "SELECT received_at, caller, provider, route_kind, client_protocol, \
                 streaming, model_requested, model_upstream, body_hash \
                 FROM requests WHERE request_id = 'req-1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .expect("fetch row");
        assert_eq!(row.0, RECEIVED_MS);
        assert_eq!(row.1, "laptop", "caller (PRX-01 key id) must persist");
        assert_eq!(row.2, "prov-a");
        assert_eq!(row.3, "messages");
        assert_eq!(row.4, "anthropic");
        assert_eq!(row.5, 1, "streaming flag stored as 1");
        assert_eq!(row.6, "claude-3");
        assert_eq!(row.7.as_deref(), Some("claude-3-upstream"));
        assert_eq!(row.8, "sha256-digest-only");
        let has_body_column: i64 = scalar(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('requests') WHERE name = 'body'",
        );
        assert_eq!(has_body_column, 0, "no request-body column may exist");

        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");
    }

    #[tokio::test]
    async fn completed_event_fills_the_same_row_with_usage() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");
        bus.emit(&received_event("req-2", "laptop"));
        bus.emit(&completed_event("req-2"));
        let conn = reader(&db);
        wait_until("completed row is filled", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests \
                 WHERE request_id = 'req-2' AND completed_at IS NOT NULL",
            ) == 1
        })
        .await;

        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM requests"),
            1,
            "one request, one row: the terminal event updates in place"
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT input_tokens FROM requests WHERE request_id = 'req-2'"
            ),
            100
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT output_tokens FROM requests WHERE request_id = 'req-2'"
            ),
            50
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT reasoning_tokens FROM requests WHERE request_id = 'req-2'"
            ),
            7
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT cache_read_tokens FROM requests WHERE request_id = 'req-2'"
            ),
            200
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT cache_creation_tokens FROM requests WHERE request_id = 'req-2'"
            ),
            30
        );
        assert_eq!(
            text_column(
                &conn,
                "SELECT usage_provenance FROM requests WHERE request_id = 'req-2'"
            )
            .as_deref(),
            Some("provider_reported")
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT received_at FROM requests WHERE request_id = 'req-2'"
            ),
            RECEIVED_MS,
            "the terminal event must not clobber received_at"
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT completed_at FROM requests WHERE request_id = 'req-2'"
            ),
            COMPLETED_MS
        );

        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");
    }

    #[tokio::test]
    async fn completed_event_persists_exact_cost_and_stop_reason() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");
        bus.emit(&received_event("req-cost", "laptop"));
        bus.emit(&completed_event("req-cost"));
        let conn = reader(&db);
        wait_until("completed row carries its cost", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests \
                 WHERE request_id = 'req-cost' AND cost_total IS NOT NULL",
            ) == 1
        })
        .await;

        let cost_input_text = text_column(
            &conn,
            "SELECT cost_input FROM requests WHERE request_id = 'req-cost'",
        )
        .expect("cost_input stored");
        assert_eq!(
            Decimal::from_str_exact(&cost_input_text).expect("cost parses back"),
            Decimal::from_str_exact("0.000123456789012345678901234").expect("original"),
            "cost must round-trip exactly through its stored text"
        );
        assert_eq!(
            text_column(
                &conn,
                "SELECT cost_total FROM requests WHERE request_id = 'req-cost'"
            )
            .as_deref(),
            Some("2.250123956789012345678901234")
        );
        assert_eq!(
            text_column(
                &conn,
                "SELECT stop_reason FROM requests WHERE request_id = 'req-cost'"
            )
            .as_deref(),
            Some("unknown:weird-finish"),
            "unknown stop reasons keep their provider value"
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT latency_ms FROM requests WHERE request_id = 'req-cost'"
            ),
            4321
        );

        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");
    }

    #[tokio::test]
    async fn failed_event_upserts_and_preserves_known_fields() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");

        bus.emit(&received_event("req-3", "laptop"));
        bus.emit(&failed_event("req-3", None));
        bus.emit(&failed_event("orphan-1", Some("prov-b")));

        let conn = reader(&db);
        wait_until("both failure rows exist", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests \
                 WHERE request_id IN ('req-3', 'orphan-1') AND error_kind IS NOT NULL",
            ) == 2
        })
        .await;

        assert_eq!(
            text_column(
                &conn,
                "SELECT provider FROM requests WHERE request_id = 'req-3'"
            )
            .as_deref(),
            Some("prov-a"),
            "a None provider on failure must not NULL out the stored provider"
        );
        assert_eq!(
            text_column(
                &conn,
                "SELECT error_kind FROM requests WHERE request_id = 'req-3'"
            )
            .as_deref(),
            Some("UpstreamError")
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT http_status FROM requests WHERE request_id = 'req-3'"
            ),
            502
        );

        let orphan_received: Option<i64> = conn
            .query_row(
                "SELECT received_at FROM requests WHERE request_id = 'orphan-1'",
                [],
                |row| row.get(0),
            )
            .expect("orphan row");
        assert_eq!(
            orphan_received, None,
            "a failure-only row has no received timestamp to report"
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT completed_at FROM requests WHERE request_id = 'orphan-1'"
            ),
            COMPLETED_MS
        );
        assert_eq!(
            text_column(
                &conn,
                "SELECT provider FROM requests WHERE request_id = 'orphan-1'"
            )
            .as_deref(),
            Some("prov-b")
        );
        let has_message_column: i64 = scalar(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('requests') WHERE name = 'message'",
        );
        assert_eq!(has_message_column, 0);

        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");
    }

    #[test]
    fn emit_counts_drops_when_channel_is_full_and_never_blocks() {
        let bus = SqliteEventBus::unconnected(8);
        for i in 0..20 {
            bus.emit(&received_event(&format!("req-{i}"), "laptop"));
        }
        assert_eq!(bus.dropped_events(), 12, "20 emits into capacity 8 drop 12");
    }

    #[tokio::test]
    async fn every_emitted_event_is_persisted_or_counted_never_lost() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = Arc::new(
            SqliteEventBus::open_with_capacity(&db, unbounded(), 32).expect("open ledger"),
        );
        for i in 0..300 {
            bus.emit(&received_event(&format!("r{i}"), "laptop"));
        }
        let closer = Arc::clone(&bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");

        let conn = reader(&db);
        let persisted = scalar(&conn, "SELECT COUNT(*) FROM requests");
        assert_eq!(
            persisted + i64::try_from(bus.dropped_events()).expect("u64 fits i64"),
            300,
            "every event is either persisted or counted as dropped — never lost silently"
        );
    }

    #[tokio::test]
    async fn close_flushes_everything_queued_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = Arc::new(SqliteEventBus::open(&db, unbounded()).expect("open ledger"));
        for i in 0..25 {
            bus.emit(&received_event(&format!("f{i}"), "laptop"));
        }
        let closer = Arc::clone(&bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");

        let conn = reader(&db);
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM requests"),
            25,
            "close() returns only after the queue is fully drained"
        );
        assert_eq!(bus.dropped_events(), 0);
    }

    #[tokio::test]
    async fn retention_pass_prunes_by_age_then_row_cap() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(&db, unbounded()).expect("open ledger");
        for i in 0..5 {
            bus.emit(&received_event(&format!("p{i}"), "laptop"));
        }
        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close");

        let mut conn = Connection::open(&db).expect("open ledger");
        wait_until("all five rows visible", || {
            scalar(&conn, "SELECT COUNT(*) FROM requests") == 5
        })
        .await;
        let now_ms = unix_millis(OffsetDateTime::now_utc());
        for (id, ts) in [
            ("p0", 1000_i64),
            ("p1", 2000),
            ("p2", now_ms - 60_000),
            ("p3", now_ms - 50_000),
            ("p4", now_ms - 40_000),
        ] {
            conn.execute(
                "UPDATE requests SET received_at = ?1 WHERE request_id = ?2",
                rusqlite::params![ts, id],
            )
            .expect("backdate row");
        }

        let deleted = run_retention_pass(
            &mut conn,
            &RetentionPolicy {
                max_age: Some(Duration::from_secs(3600)),
                max_rows: None,
                prune_every: Duration::from_secs(3600),
            },
        )
        .expect("age pass");
        assert_eq!(deleted, 2, "the two ancient rows are pruned by age");
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM requests"), 3);

        let deleted = run_retention_pass(
            &mut conn,
            &RetentionPolicy {
                max_age: None,
                max_rows: Some(1),
                prune_every: Duration::from_secs(3600),
            },
        )
        .expect("row-cap pass");
        assert_eq!(deleted, 2, "the two oldest of the remaining rows go");
        assert_eq!(
            text_column(&conn, "SELECT request_id FROM requests"),
            Some("p4".to_owned()),
            "the newest row survives the cap"
        );

        let deleted = run_retention_pass(&mut conn, &unbounded()).expect("no-op pass");
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn retention_timer_prunes_old_rows_without_manual_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let _bus = SqliteEventBus::open(
            &db,
            RetentionPolicy {
                max_age: Some(Duration::from_secs(3600)),
                max_rows: None,
                prune_every: Duration::from_secs(1),
            },
        )
        .expect("open ledger");
        let ProxyEvent::RequestReceived(mut fresh) = received_event("stale", "laptop") else {
            panic!("builder returns RequestReceived");
        };
        fresh.timestamp = OffsetDateTime::now_utc();
        _bus.emit(&ProxyEvent::RequestReceived(fresh));

        let conn = reader(&db);
        wait_until("row appears", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests WHERE request_id = 'stale'",
            ) == 1
        })
        .await;
        conn.execute(
            "UPDATE requests SET received_at = 1000 WHERE request_id = 'stale'",
            [],
        )
        .expect("backdate row");
        wait_until("timer prunes the stale row", || {
            scalar(
                &conn,
                "SELECT COUNT(*) FROM requests WHERE request_id = 'stale'",
            ) == 0
        })
        .await;
    }
}
