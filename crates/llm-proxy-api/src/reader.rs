use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::types::Value as SqlValue;
use serde::Serialize;
use time::OffsetDateTime;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const MAX_PAGE_LIMIT: i64 = 1000;

#[derive(Debug, thiserror::Error)]
pub(crate) enum UsageQueryError {
    #[error("usage ledger query failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("usage ledger cost value is not a valid decimal: {0}")]
    Cost(String),
    #[error("usage ledger query task failed: {0}")]
    Task(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupBy {
    Provider,
    Model,
    Caller,
}

impl GroupBy {
    const fn column(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Model => "model_requested",
            Self::Caller => "caller",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "provider" => Some(Self::Provider),
            "model" => Some(Self::Model),
            "caller" => Some(Self::Caller),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Caller => "caller",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct TokenCounts {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CostTotals {
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_creation: String,
    pub reasoning: String,
    pub total: String,
}

impl CostTotals {
    fn zeroed() -> Self {
        Self {
            input: "0".to_owned(),
            output: "0".to_owned(),
            cache_read: "0".to_owned(),
            cache_creation: "0".to_owned(),
            reasoning: "0".to_owned(),
            total: "0".to_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SummaryGroup {
    pub key: Option<String>,
    pub requests: i64,
    pub errors: i64,
    pub tokens: TokenCounts,
    pub cost: CostTotals,
}

#[derive(Debug, Serialize)]
pub(crate) struct RequestRow {
    pub request_id: String,
    pub received_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub caller: Option<String>,
    pub provider: Option<String>,
    pub route_kind: Option<String>,
    pub client_protocol: Option<String>,
    pub streaming: Option<bool>,
    pub model_requested: Option<String>,
    pub model_upstream: Option<String>,
    pub tokens: Option<TokenCounts>,
    pub usage_provenance: Option<String>,
    pub cost: Option<CostTotals>,
    pub stop_reason: Option<String>,
    pub error_kind: Option<String>,
    pub http_status: Option<i64>,
    pub latency_ms: Option<i64>,
    pub body_hash: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RequestsPage {
    pub rows: Vec<RequestRow>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CallerSummary {
    pub caller: Option<String>,
    pub requests: i64,
    pub errors: i64,
    pub last_seen_ms: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct RequestFilter {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub provider: Option<String>,
    pub limit: i64,
    pub cursor: Option<(i64, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageReader {
    path: PathBuf,
}

fn offset_to_unix_millis(ts: OffsetDateTime) -> i64 {
    ts.unix_timestamp()
        .saturating_mul(1000)
        .saturating_add(i64::from(ts.millisecond()))
}

pub(crate) fn now_unix_millis() -> i64 {
    offset_to_unix_millis(OffsetDateTime::now_utc())
}

impl UsageReader {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) async fn summary(
        &self,
        since_ms: i64,
        group: GroupBy,
    ) -> Result<Vec<SummaryGroup>, UsageQueryError> {
        let path = self.path.clone();
        run_blocking(move || summary_sync(&path, since_ms, group)).await
    }

    pub(crate) async fn requests(
        &self,
        filter: RequestFilter,
    ) -> Result<RequestsPage, UsageQueryError> {
        let path = self.path.clone();
        run_blocking(move || requests_sync(&path, &filter)).await
    }

    pub(crate) async fn callers(&self) -> Result<Vec<CallerSummary>, UsageQueryError> {
        let path = self.path.clone();
        run_blocking(move || callers_sync(&path)).await
    }

    fn open(path: &Path) -> Result<Connection, UsageQueryError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        Ok(conn)
    }
}

async fn run_blocking<T, F>(f: F) -> Result<T, UsageQueryError>
where
    F: FnOnce() -> Result<T, UsageQueryError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| UsageQueryError::Task(error.to_string()))?
}

fn parse_cost(value: Option<String>) -> Result<rust_decimal::Decimal, UsageQueryError> {
    match value {
        None => Ok(rust_decimal::Decimal::ZERO),
        Some(text) => rust_decimal::Decimal::from_str_exact(&text)
            .map_err(|_parse_error| UsageQueryError::Cost(text)),
    }
}

fn summary_sync(
    path: &Path,
    since_ms: i64,
    group: GroupBy,
) -> Result<Vec<SummaryGroup>, UsageQueryError> {
    let conn = UsageReader::open(path)?;
    let key = group.column();
    let sql = format!(
        "SELECT {key} AS k, COUNT(*), \
         SUM(CASE WHEN error_kind IS NOT NULL THEN 1 ELSE 0 END), \
         COALESCE(SUM(COALESCE(input_tokens, 0)), 0), \
         COALESCE(SUM(COALESCE(output_tokens, 0)), 0), \
         COALESCE(SUM(COALESCE(reasoning_tokens, 0)), 0), \
         COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0), \
         COALESCE(SUM(COALESCE(cache_creation_tokens, 0)), 0) \
         FROM requests \
         WHERE COALESCE(received_at, completed_at) >= ?1 \
         GROUP BY {key} ORDER BY 2 DESC"
    );
    let mut groups: Vec<SummaryGroup> = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([since_ms], |row| {
            Ok(SummaryGroup {
                key: row.get(0)?,
                requests: row.get(1)?,
                errors: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                tokens: TokenCounts {
                    input: row.get(3)?,
                    output: row.get(4)?,
                    reasoning: row.get(5)?,
                    cache_read: row.get(6)?,
                    cache_creation: row.get(7)?,
                },
                cost: CostTotals::zeroed(),
            })
        })?;
        for row in rows {
            groups.push(row?);
        }
    }
    let costs = sum_costs_by_group(&conn, key, since_ms)?;
    for group_row in &mut groups {
        if let Some(total) = costs.get(&group_row.key) {
            group_row.cost = cost_totals_from(*total);
        }
    }
    Ok(groups)
}

fn sum_costs_by_group(
    conn: &Connection,
    key: &str,
    since_ms: i64,
) -> Result<HashMap<Option<String>, [rust_decimal::Decimal; 6]>, UsageQueryError> {
    let sql = format!(
        "SELECT {key} AS k, cost_input, cost_output, cost_cache_read, \
         cost_cache_creation, cost_reasoning, cost_total \
         FROM requests \
         WHERE COALESCE(received_at, completed_at) >= ?1 AND cost_total IS NOT NULL"
    );
    let mut sums: HashMap<Option<String>, [rust_decimal::Decimal; 6]> = HashMap::new();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([since_ms], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    for row in rows {
        let (key, input, output, cache_read, cache_creation, reasoning, total) = row?;
        let entry = sums.entry(key).or_insert([rust_decimal::Decimal::ZERO; 6]);
        let parts = [
            parse_cost(input)?,
            parse_cost(output)?,
            parse_cost(cache_read)?,
            parse_cost(cache_creation)?,
            parse_cost(reasoning)?,
            parse_cost(total)?,
        ];
        for (slot, part) in entry.iter_mut().zip(parts) {
            *slot += part;
        }
    }
    Ok(sums)
}

fn cost_totals_from(sums: [rust_decimal::Decimal; 6]) -> CostTotals {
    let show = |d: rust_decimal::Decimal| d.normalize().to_string();
    CostTotals {
        input: show(sums[0]),
        output: show(sums[1]),
        cache_read: show(sums[2]),
        cache_creation: show(sums[3]),
        reasoning: show(sums[4]),
        total: show(sums[5]),
    }
}

fn map_request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(RequestRow, i64)> {
    let input: Option<i64> = row.get(10)?;
    let output: Option<i64> = row.get(11)?;
    let reasoning: Option<i64> = row.get(12)?;
    let cache_read: Option<i64> = row.get(13)?;
    let cache_creation: Option<i64> = row.get(14)?;
    let cost_input: Option<String> = row.get(16)?;
    let cost_output: Option<String> = row.get(17)?;
    let cost_cache_creation: Option<String> = row.get(18)?;
    let cost_cache_read: Option<String> = row.get(19)?;
    let cost_reasoning: Option<String> = row.get(20)?;
    let cost_total: Option<String> = row.get(21)?;
    let streaming: Option<i64> = row.get(7)?;
    let tokens = input.map(|input| TokenCounts {
        input,
        output: output.unwrap_or(0),
        reasoning: reasoning.unwrap_or(0),
        cache_read: cache_read.unwrap_or(0),
        cache_creation: cache_creation.unwrap_or(0),
    });
    let cost = cost_total.map(|total| CostTotals {
        input: cost_input.unwrap_or_else(|| "0".to_owned()),
        output: cost_output.unwrap_or_else(|| "0".to_owned()),
        cache_read: cost_cache_read.unwrap_or_else(|| "0".to_owned()),
        cache_creation: cost_cache_creation.unwrap_or_else(|| "0".to_owned()),
        reasoning: cost_reasoning.unwrap_or_else(|| "0".to_owned()),
        total,
    });
    let row_out = RequestRow {
        request_id: row.get(0)?,
        received_at_ms: row.get(1)?,
        completed_at_ms: row.get(2)?,
        caller: row.get(3)?,
        provider: row.get(4)?,
        route_kind: row.get(5)?,
        client_protocol: row.get(6)?,
        streaming: streaming.map(|flag| flag != 0),
        model_requested: row.get(8)?,
        model_upstream: row.get(9)?,
        tokens,
        usage_provenance: row.get(15)?,
        cost,
        stop_reason: row.get(22)?,
        error_kind: row.get(23)?,
        http_status: row.get(24)?,
        latency_ms: row.get(25)?,
        body_hash: row.get(26)?,
    };
    let ts: Option<i64> = row.get(27)?;
    Ok((row_out, ts.unwrap_or(i64::MIN)))
}

fn requests_sync(path: &Path, filter: &RequestFilter) -> Result<RequestsPage, UsageQueryError> {
    let conn = UsageReader::open(path)?;
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(since) = filter.since_ms {
        clauses.push("COALESCE(received_at, completed_at) >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = filter.until_ms {
        clauses.push("COALESCE(received_at, completed_at) < ?");
        params.push(SqlValue::Integer(until));
    }
    if let Some(provider) = &filter.provider {
        clauses.push("provider = ?");
        params.push(SqlValue::Text(provider.clone()));
    }
    if let Some((cursor_ts, cursor_id)) = &filter.cursor {
        clauses.push(
            "(COALESCE(received_at, completed_at) < ? \
              OR (COALESCE(received_at, completed_at) = ? AND request_id < ?))",
        );
        params.push(SqlValue::Integer(*cursor_ts));
        params.push(SqlValue::Integer(*cursor_ts));
        params.push(SqlValue::Text(cursor_id.clone()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT request_id, received_at, completed_at, caller, provider, route_kind, \
         client_protocol, streaming, model_requested, model_upstream, input_tokens, \
         output_tokens, reasoning_tokens, cache_read_tokens, cache_creation_tokens, \
         usage_provenance, cost_input, cost_output, cost_cache_creation, cost_cache_read, \
         cost_reasoning, cost_total, stop_reason, error_kind, http_status, latency_ms, \
         body_hash, COALESCE(received_at, completed_at) AS ts \
         FROM requests {where_sql} \
         ORDER BY ts DESC, request_id DESC \
         LIMIT ?"
    );
    params.push(SqlValue::Integer(filter.limit + 1));
    let mut fetched: Vec<(RequestRow, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), map_request_row)?;
        for row in rows {
            fetched.push(row?);
        }
    }
    let has_more = fetched.len() > usize::try_from(filter.limit).unwrap_or(0);
    if has_more {
        fetched.truncate(usize::try_from(filter.limit).unwrap_or(0));
    }
    let next_cursor = if has_more {
        fetched
            .last()
            .map(|(row, ts)| format!("{ts}:{}", row.request_id))
    } else {
        None
    };
    Ok(RequestsPage {
        rows: fetched.into_iter().map(|(row, _ts)| row).collect(),
        next_cursor,
    })
}

fn callers_sync(path: &Path) -> Result<Vec<CallerSummary>, UsageQueryError> {
    let conn = UsageReader::open(path)?;
    let mut stmt = conn.prepare(
        "SELECT caller, COUNT(*), \
         SUM(CASE WHEN error_kind IS NOT NULL THEN 1 ELSE 0 END), \
         MAX(COALESCE(received_at, completed_at)) \
         FROM requests GROUP BY caller ORDER BY 4 DESC NULLS LAST",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CallerSummary {
            caller: row.get(0)?,
            requests: row.get(1)?,
            errors: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            last_seen_ms: row.get(3)?,
        })
    })?;
    let mut callers = Vec::new();
    for row in rows {
        callers.push(row?);
    }
    Ok(callers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_connections_refuse_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("usage.db");
        {
            let writer = Connection::open(&db).expect("create db");
            writer
                .execute_batch("CREATE TABLE t (a INTEGER);")
                .expect("seed table");
        }
        let reader = UsageReader::open(&db).expect("open read-only");
        let attempt = reader.execute("INSERT INTO t (a) VALUES (1)", []);
        assert!(
            attempt.is_err(),
            "SQLITE_OPEN_READ_ONLY must refuse writes at the connection level"
        );
        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .expect("read on the read-only connection");
        assert_eq!(count, 0);
    }

    #[test]
    fn group_by_parses_known_values_and_rejects_unknown() {
        assert_eq!(GroupBy::parse("provider"), Some(GroupBy::Provider));
        assert_eq!(GroupBy::parse("model"), Some(GroupBy::Model));
        assert_eq!(GroupBy::parse("caller"), Some(GroupBy::Caller));
        assert_eq!(GroupBy::parse("team"), None);
        assert_eq!(GroupBy::Provider.as_str(), "provider");
        assert_eq!(GroupBy::Model.as_str(), "model");
        assert_eq!(GroupBy::Caller.as_str(), "caller");
    }
}
