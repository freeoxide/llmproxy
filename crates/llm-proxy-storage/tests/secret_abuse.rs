//! Secret-abuse family (PRX-06) for the SQLite ledger: only the documented
//! columns ever reach the raw file bytes. The `ResponseFailed.message` field
//! can carry sanitized upstream text (truncated error bodies), so the abuse
//! property is that whatever an event's message holds, it is never persisted
//! — a raw-bytes assertion, stronger than checking the column list, because
//! it scans every byte of the database and its WAL sidecars, exactly what an
//! attacker with filesystem access would read.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use llm_proxy_protocol::core::{Cost, ModelRef, StopReason, Usage, UsageProvenance};
    use llm_proxy_storage::{
        EventBus, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed, SqliteEventBus,
    };
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    /// Distinctive markers, unique to this family so a hit can never be a
    /// coincidence with unrelated file content.
    const CALLER_MARKER: &str = "prx6-storage-caller-77aa01";
    const DIGEST_MARKER: &str = "prx6-storage-digest-88bb02";
    const STOP_LABEL_CONTROL: &str = "end_turn";
    /// Marker placed in `ResponseFailed.message` — event-carried text that
    /// must never be persisted.
    const MESSAGE_MARKER: &str = "prx6-storage-message-99cc03";

    fn received(ts: OffsetDateTime) -> ProxyEvent {
        ProxyEvent::RequestReceived(RequestReceived {
            request_id: "prx6-row-1".to_owned(),
            timestamp: ts,
            caller: CALLER_MARKER.to_owned(),
            provider: "prov".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: ModelRef {
                requested: "claude-3".to_owned(),
                upstream: None,
            },
            streaming: false,
            body_hash: DIGEST_MARKER.to_owned(),
        })
    }

    fn completed(ts: OffsetDateTime) -> ProxyEvent {
        ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: "prx6-row-1".to_owned(),
            timestamp: ts,
            provider: "prov".to_owned(),
            upstream_message_id: Some("msg_upstream".to_owned()),
            model: ModelRef {
                requested: "claude-3".to_owned(),
                upstream: None,
            },
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                reasoning_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                provenance: UsageProvenance::ProviderReported,
            },
            cost: Some(Cost {
                input: Decimal::from(11),
                output: Decimal::ZERO,
                cache_creation: Decimal::ZERO,
                cache_read: Decimal::ZERO,
                reasoning: Decimal::ZERO,
                total: Decimal::from(11),
            }),
            stop_reason: StopReason::EndTurn,
            latency_ms: 42,
        })
    }

    fn failed(request_id: &str, ts: OffsetDateTime) -> ProxyEvent {
        ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: request_id.to_owned(),
            timestamp: ts,
            provider: Some("prov".to_owned()),
            model: None,
            error_kind: "UpstreamError".to_owned(),
            // The sanitized message stands in for any event-carried text:
            // upstream error fragments, echoes, anything. None of it may be
            // persisted, even for a failure whose request row exists.
            message: format!("upstream failed after echoing {MESSAGE_MARKER}"),
            http_status: 502,
            latency_ms: 5,
        })
    }

    /// Concatenated raw bytes of the ledger database and any WAL/SHM
    /// sidecars still on disk.
    fn ledger_bytes(db: &Path) -> Vec<u8> {
        let mut all = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", db.display()));
            if let Ok(bytes) = std::fs::read(&sidecar) {
                all.extend_from_slice(&bytes);
            }
        }
        all
    }

    #[tokio::test]
    async fn raw_ledger_bytes_persist_columns_but_never_event_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("abuse.db");
        let bus = SqliteEventBus::open(
            &db,
            llm_proxy_storage::RetentionPolicy {
                max_age: None,
                max_rows: None,
                prune_every: Duration::from_secs(3600),
            },
        )
        .expect("ledger opens");

        let t0 = OffsetDateTime::now_utc();
        // A completed request whose terminal failure text carries the marker
        // (a paired row that gets both INSERT and UPDATE), plus a
        // failure-only orphan row whose message also carries it.
        bus.emit(&received(t0));
        bus.emit(&failed("prx6-row-1", t0));
        bus.emit(&completed(t0));
        bus.emit(&failed("prx6-orphan", t0));

        let closer = std::sync::Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close joins");

        let bytes = ledger_bytes(&db);
        let text = String::from_utf8_lossy(&bytes);
        // Positive controls: the scan finds values that ARE persisted — the
        // caller key id, the body digest, and a stop-reason label — proving
        // the negative assertion below reads the real rows.
        for control in [CALLER_MARKER, DIGEST_MARKER, STOP_LABEL_CONTROL] {
            assert!(
                text.contains(control),
                "control: persisted value {control} must be findable in the raw bytes"
            );
        }
        assert!(
            !text.contains(MESSAGE_MARKER),
            "event message text (upstream echoes, error fragments) leaked into the \
             raw ledger file bytes"
        );
    }
}
