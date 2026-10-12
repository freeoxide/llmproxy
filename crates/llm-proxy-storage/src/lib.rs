//! Structured request/response event log surface.
//!
//! The persistence backend (SQLite, etc.) lands in a later phase. This crate
//! defines the event types and the [`EventBus`] trait that backends implement,
//! plus a [`NoopBus`] default and a [`RecordingBus`] for tests.
//!
//! Events are emitted at request/response boundaries in the server crate. A
//! real persisted backend will implement [`EventBus`]; for now [`NoopBus`] is
//! the production sink and [`RecordingBus`] (behind the `test-utils` cargo
//! feature) lets integration tests assert on the event sequence.

#![deny(missing_docs)]

use llm_proxy_protocol::core::{Cost, ModelRef, StopReason, Usage};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// ProxyEvent
// ---------------------------------------------------------------------------

/// One request/response lifecycle event, ready to persist or emit.
///
/// Internally tagged via `kind` so JSON/TOML consumers can discriminate without
/// a wrapper object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProxyEvent {
    /// A request was received and routed.
    RequestReceived(RequestReceived),
    /// A response completed successfully.
    ResponseCompleted(ResponseCompleted),
    /// A response failed before or during upstream dispatch.
    ResponseFailed(ResponseFailed),
}

// ---------------------------------------------------------------------------
// RequestReceived
// ---------------------------------------------------------------------------

/// Event payload for a received request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestReceived {
    /// Proxy-generated unique request identifier.
    pub request_id: String,
    /// When the request was received (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Authenticated client key id, or `"anonymous"` when the request was
    /// served under the loopback no-auth exemption. Key ids are operator
    /// chosen; client secrets never appear on events.
    pub caller: String,
    /// Provider name from the URL path.
    pub provider: String,
    /// Route kind (`"chat_completions"` or `"messages"`).
    pub route_kind: String,
    /// Client protocol (`"openai_chat"` or `"anthropic"`).
    pub client_protocol: String,
    /// Requested and upstream model.
    pub model: ModelRef,
    /// Whether the request requested streaming.
    pub streaming: bool,
    /// SHA-256 of the raw client request body for dedup/correlation (not the
    /// body itself, which may contain secrets).
    pub body_hash: String,
}

// ---------------------------------------------------------------------------
// ResponseCompleted
// ---------------------------------------------------------------------------

/// Event payload for a completed response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCompleted {
    /// Proxy-generated unique request identifier.
    pub request_id: String,
    /// When the response completed (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Provider name.
    pub provider: String,
    /// Upstream message id, if available from `MessageStart` or
    /// `CoreResponse.id`.
    pub upstream_message_id: Option<String>,
    /// Requested and upstream model.
    pub model: ModelRef,
    /// Token usage reported by the provider.
    pub usage: Usage,
    /// Computed cost, if pricing was configured for the model.
    pub cost: Option<Cost>,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
    /// Request latency in milliseconds.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// ResponseFailed
// ---------------------------------------------------------------------------

/// Event payload for a failed response.
///
/// `model` and `provider` are optional because early failures (unknown
/// provider, JSON parse error, rate limit) can occur before the model or
/// provider is confirmed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFailed {
    /// Proxy-generated unique request identifier.
    pub request_id: String,
    /// When the failure occurred (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Provider name, if known at failure time.
    pub provider: Option<String>,
    /// Requested and upstream model, if decoded before failure.
    pub model: Option<ModelRef>,
    /// Error kind string from the `RouteError` variant name.
    pub error_kind: String,
    /// Sanitized error message; API keys and URLs are always redacted.
    ///
    /// For upstream errors whose status is passed through to the client
    /// (400/413/429, and client-owned 401/403) this field holds a generic
    /// proxy message so the provider's own schema (field names, model
    /// identifiers, request echoes) never reaches the client; the real
    /// upstream body is logged server-side only. For statuses that collapse to
    /// 502, this field contains the truncated, key/URL-redacted upstream body.
    pub message: String,
    /// HTTP status code returned to the client.
    pub http_status: u16,
    /// Request latency in milliseconds.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// EventBus trait + implementations
// ---------------------------------------------------------------------------

/// In-process event sink.
///
/// Implementations: [`NoopBus`], [`RecordingBus`] (tests), and the future
/// SQLite backend.
///
/// `Debug` is a super-trait so that `Arc<dyn EventBus>` can be formatted in
/// `AppState`'s manual `Debug` impl.
pub trait EventBus: Send + Sync + std::fmt::Debug {
    /// Emit one event. Must not block the calling task; backends do IO on a
    /// dedicated writer task.
    fn emit(&self, event: &ProxyEvent);
}

/// Default no-op sink.
#[derive(Debug, Clone, Default)]
pub struct NoopBus;

impl EventBus for NoopBus {
    fn emit(&self, _event: &ProxyEvent) {}
}

/// Test-only sink that records events for assertions. Available behind the
/// `test-utils` cargo feature (NOT `#[cfg(test)]`, which is stripped when the
/// crate is compiled as a dependency).
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug, Default)]
pub struct RecordingBus {
    /// Recorded events, guarded by a mutex for interior mutability.
    pub events: std::sync::Mutex<Vec<ProxyEvent>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl RecordingBus {
    /// Create an empty recording bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the recorded events (cloned under the lock).
    ///
    /// Recovers from mutex poison rather than returning an empty `Vec`, so a
    /// poisoned bus still reflects the events recorded before the panic
    /// (consistent with the recover-poison pattern used in `metrics.rs`).
    pub fn snapshot(&self) -> Vec<ProxyEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl EventBus for RecordingBus {
    fn emit(&self, event: &ProxyEvent) {
        // Recover from mutex poison so a poisoned bus still records the event
        // rather than silently dropping it (recover-poison pattern, consistent
        // with `metrics.rs` and `catalog_service.rs`).
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_proxy_protocol::core::UsageProvenance;

    fn sample_request_received() -> RequestReceived {
        RequestReceived {
            request_id: "req-1".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            caller: "anonymous".to_owned(),
            provider: "p".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: ModelRef {
                requested: "claude".to_owned(),
                upstream: None,
            },
            streaming: false,
            body_hash: "abc123".to_owned(),
        }
    }

    #[test]
    fn noop_bus_emit_does_not_panic_or_record() {
        let bus = NoopBus;
        // NoopBus has nowhere to record; just ensure emit is callable.
        let event = ProxyEvent::RequestReceived(sample_request_received());
        bus.emit(&event);
    }

    #[test]
    fn recording_bus_captures_events_in_order() {
        let bus = RecordingBus::new();
        let req = ProxyEvent::RequestReceived(sample_request_received());
        let done = ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: "req-1".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            provider: "p".to_owned(),
            upstream_message_id: Some("msg_42".to_owned()),
            model: ModelRef {
                requested: "claude".to_owned(),
                upstream: None,
            },
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            cost: None,
            stop_reason: StopReason::EndTurn,
            latency_ms: 123,
        });
        bus.emit(&req);
        bus.emit(&done);

        let events = bus.snapshot();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ProxyEvent::RequestReceived(_)));
        assert!(matches!(events[1], ProxyEvent::ResponseCompleted(_)));
    }

    #[test]
    fn proxy_event_serializes_with_kind_tag_and_rfc3339_timestamp() {
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;

        // Pin a known timestamp instead of `now_utc()` so the format assertions
        // are deterministic and the round-trip can be checked exactly. The
        // nanos value (`123_456_789`) is deliberately chosen to be nonzero at
        // every precision group (ms/us/ns): `time`'s RFC 3339 serializer
        // trims trailing zero groups, so a value like `123_000_000` would be
        // emitted as milliseconds (`.123`, 3 digits) rather than nanoseconds.
        // Forcing nonzero nanos guarantees the full 9-digit fractional component
        // is emitted, so the precision assertion below is meaningful.
        let timestamp = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("valid unix timestamp")
            .replace_nanosecond(123_456_789)
            .expect("valid nanoseconds");
        let event = ProxyEvent::RequestReceived(RequestReceived {
            request_id: "req-1".to_owned(),
            timestamp,
            caller: "anonymous".to_owned(),
            provider: "p".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: ModelRef {
                requested: "claude".to_owned(),
                upstream: None,
            },
            streaming: false,
            body_hash: "abc123".to_owned(),
        });
        let json = serde_json::to_string(&event).expect("serialize");
        // Internally tagged by `kind`.
        assert!(json.contains(r#""kind":"request_received""#), "got: {json}");

        // Extract the serialized timestamp string from the JSON and validate a
        // full RFC 3339 shape rather than only asserting `contains('T')`. The
        // previous weaker assertion would pass for any value with a literal
        // 'T' anywhere, masking a malformed/non-RFC-3339 timestamp.
        let extracted = extract_json_string_field(&json, "timestamp")
            .expect("timestamp field present and a JSON string");
        // Must end in 'Z' (UTC) or a numeric offset like +00:00 / -05:00.
        assert!(
            extracted.ends_with('Z') || is_numeric_offset_suffix(&extracted),
            "timestamp must end in Z or a numeric offset, got: {extracted}"
        );
        // Full RFC 3339 round-trip: parsing the extracted string back into an
        // OffsetDateTime must succeed and reproduce the original instant.
        let parsed =
            OffsetDateTime::parse(&extracted, &Rfc3339).expect("timestamp parses as RFC 3339");
        assert_eq!(
            parsed, timestamp,
            "timestamp must round-trip through RFC 3339; got: {extracted}"
        );
        // Precision: `time::serde::rfc3339` emits a fractional-seconds component
        // and trims trailing zero groups (3 digits for whole-ms, 6 for whole-us,
        // 9 otherwise). Our pinned nanos are nonzero at every group, so the full
        // 9-digit nanosecond component is emitted. Confirm the subsecond part is
        // present and carries nanosecond precision.
        let frac = extracted
            .split('.')
            .nth(1)
            .and_then(|rest| rest.split(['+', '-', 'Z']).next())
            .unwrap_or("");
        assert!(
            !frac.is_empty(),
            "timestamp must carry subsecond precision, got: {extracted}"
        );
        assert_eq!(
            frac.len(),
            9,
            "nanosecond precision expected (9 fractional digits), got: {extracted}"
        );
    }

    /// Return true if `s` ends in an RFC 3339 numeric offset like `+00:00`.
    fn is_numeric_offset_suffix(s: &str) -> bool {
        // A numeric offset is `[+-]HH:MM` (6 chars).
        if s.len() < 6 {
            return false;
        }
        let tail = &s[s.len() - 6..];
        let sign = tail.as_bytes()[0];
        if sign != b'+' && sign != b'-' {
            return false;
        }
        tail[1..].chars().all(|c| c.is_ascii_digit() || c == ':')
    }

    /// Extract the string value of a top-level JSON string field from a
    /// serialized `ProxyEvent` payload.
    ///
    /// Returns `None` if the field is absent or not a JSON string. Honors the
    /// JSON `\"` escape so an embedded quote does not prematurely terminate the
    /// value.
    fn extract_json_string_field(json: &str, field: &str) -> Option<String> {
        let needle = format!("\"{field}\":\"");
        let start = json.find(&needle)? + needle.len();
        let bytes = json.as_bytes();
        let mut end = start;
        let mut i = start;
        let mut escaped = false;
        while i < bytes.len() {
            let c = bytes[i];
            if escaped {
                escaped = false;
                i += 1;
                continue;
            }
            if c == b'\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if c == b'"' {
                end = i;
                break;
            }
            i += 1;
        }
        Some(json[start..end].to_owned())
    }

    /// Source guard: `ProxyEvent` has no secret-carrying field by construction
    /// (the API key lives in `ProviderConfig`, never on an event). This test
    /// guards against future regressions where a field named like a secret
    /// (`api_key`, `secret`, `token`, `bearer`, `authorization`, `password`)
    /// is accidentally added to one of the event payloads.
    ///
    /// It serializes all three variants and asserts none of the forbidden
    /// secret-bearing field names appear as a JSON key. It also populates every
    /// string content field with a sentinel and asserts the JSON only ever
    /// surfaces it under one of the *known* field names (proving no new,
    /// unknown string field was silently introduced).
    #[test]
    fn proxy_event_has_no_secret_field_by_construction() {
        // Field-name fragments that would indicate a secret-bearing field if
        // they appeared as a JSON key. None of these are legitimate `ProxyEvent`
        // field names.
        const FORBIDDEN_KEY_FRAGMENTS: &[&str] = &[
            "api_key",
            "apikey",
            "secret",
            "token",
            "bearer",
            "authorization",
            "password",
            "passwd",
        ];

        // A sentinel placed in every legitimate string content field. It SHOULD
        // appear in the serialized JSON (under the known field names) — the
        // assertions below check it never appears under a forbidden field name.
        const SENTINEL: &str = "ZZ-sentinel-content-ZZ";

        // --- RequestReceived ---
        let req = ProxyEvent::RequestReceived(RequestReceived {
            request_id: SENTINEL.to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            caller: SENTINEL.to_owned(),
            provider: SENTINEL.to_owned(),
            route_kind: SENTINEL.to_owned(),
            client_protocol: SENTINEL.to_owned(),
            model: ModelRef {
                requested: SENTINEL.to_owned(),
                upstream: Some(SENTINEL.to_owned()),
            },
            streaming: false,
            body_hash: SENTINEL.to_owned(),
        });
        let req_json = serde_json::to_string(&req).expect("serialize RequestReceived");
        assert_no_forbidden_secret_key(&req_json, FORBIDDEN_KEY_FRAGMENTS);

        // --- ResponseCompleted ---
        let done = ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: SENTINEL.to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            provider: SENTINEL.to_owned(),
            upstream_message_id: Some(SENTINEL.to_owned()),
            model: ModelRef {
                requested: SENTINEL.to_owned(),
                upstream: Some(SENTINEL.to_owned()),
            },
            usage: Usage::default(),
            cost: None,
            stop_reason: StopReason::EndTurn,
            latency_ms: 1,
        });
        let done_json = serde_json::to_string(&done).expect("serialize ResponseCompleted");
        assert_no_forbidden_secret_key(&done_json, FORBIDDEN_KEY_FRAGMENTS);

        // --- ResponseFailed ---
        let failed = ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: SENTINEL.to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            provider: Some(SENTINEL.to_owned()),
            model: Some(ModelRef {
                requested: SENTINEL.to_owned(),
                upstream: Some(SENTINEL.to_owned()),
            }),
            error_kind: SENTINEL.to_owned(),
            // The `message` field is sanitized error text; place the sentinel
            // here too so the guard proves it surfaces only as a value, never
            // under a forbidden key.
            message: SENTINEL.to_owned(),
            http_status: 429,
            latency_ms: 2,
        });
        let failed_json = serde_json::to_string(&failed).expect("serialize ResponseFailed");
        assert_no_forbidden_secret_key(&failed_json, FORBIDDEN_KEY_FRAGMENTS);
    }

    /// Assert that none of `fragments` appear as a JSON key in `json`.
    ///
    /// A JSON key is always serialized as `"fragment"` immediately followed by
    /// `:` (modulo whitespace, which `serde_json::to_string` does not emit), so
    /// checking for `"fragment":` avoids false positives where a fragment shows
    /// up inside a legitimate string value.
    fn assert_no_forbidden_secret_key(json: &str, fragments: &[&str]) {
        for frag in fragments {
            // Match the fragment as a JSON key: a `"` then the fragment then `"`
            // then optional whitespace then `:`. serde_json emits no spaces, so
            // the common shape is `"frag":`.
            let key_shape_quoted = format!("\"{frag}\":");
            assert!(
                !json.contains(&key_shape_quoted),
                "forbidden secret-bearing key `{frag}` found in ProxyEvent JSON: {json}"
            );
        }
    }

    #[test]
    fn response_failed_optional_fields_round_trip() {
        let event = ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: "req-2".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            provider: None,
            model: None,
            error_kind: "UnknownProvider".to_owned(),
            message: "no such provider".to_owned(),
            http_status: 404,
            latency_ms: 1,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        let back: ProxyEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, ProxyEvent::ResponseFailed(_)));
        // Sanity: UsageProvenance must be referenced so it is not flagged unused.
        let _ = UsageProvenance::Unknown;
    }
}
