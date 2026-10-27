//! Integration tests for the `/usage` query surface over a real SQLite
//! ledger seeded through `SqliteEventBus`.

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use llm_proxy_protocol::core::{Cost, ModelRef, StopReason, Usage, UsageProvenance};
    use llm_proxy_storage::{
        EventBus, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed, SqliteEventBus,
    };
    use rust_decimal::Decimal;
    use serde_json::Value;
    use time::OffsetDateTime;
    use tower::ServiceExt;

    const ADMIN_TOKEN: &str = "test-admin-token";

    struct Ledger {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        t0: OffsetDateTime,
    }

    fn received_event(
        id: &str,
        caller: &str,
        provider: &str,
        model: &str,
        ts: OffsetDateTime,
    ) -> ProxyEvent {
        ProxyEvent::RequestReceived(RequestReceived {
            request_id: id.to_owned(),
            timestamp: ts,
            caller: caller.to_owned(),
            provider: provider.to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: ModelRef {
                requested: model.to_owned(),
                upstream: None,
            },
            streaming: true,
            body_hash: format!("sha256-digest-{id}"),
        })
    }

    fn completed_event(
        id: &str,
        provider: &str,
        model: &str,
        ts: OffsetDateTime,
        total: &str,
    ) -> ProxyEvent {
        ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: id.to_owned(),
            timestamp: ts,
            provider: provider.to_owned(),
            upstream_message_id: None,
            model: ModelRef {
                requested: model.to_owned(),
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
                input: Decimal::from_str_exact(total).expect("decimal fixture"),
                output: Decimal::ZERO,
                cache_creation: Decimal::ZERO,
                cache_read: Decimal::ZERO,
                reasoning: Decimal::ZERO,
                total: Decimal::from_str_exact(total).expect("decimal fixture"),
            }),
            stop_reason: StopReason::EndTurn,
            latency_ms: 4321,
        })
    }

    fn failed_event(id: &str, provider: Option<&str>, ts: OffsetDateTime) -> ProxyEvent {
        ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: id.to_owned(),
            timestamp: ts,
            provider: provider.map(str::to_owned),
            model: None,
            error_kind: "UpstreamError".to_owned(),
            message: "sanitized failure text".to_owned(),
            http_status: 502,
            latency_ms: 99,
        })
    }

    async fn seed() -> Ledger {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(
            &path,
            llm_proxy_storage::RetentionPolicy {
                max_age: None,
                max_rows: None,
                prune_every: std::time::Duration::from_secs(3600),
            },
        )
        .expect("open ledger");

        let t0 = OffsetDateTime::now_utc().saturating_sub(time::Duration::seconds(60));
        let second = std::time::Duration::from_secs(1);
        let events = [
            received_event("req-1", "laptop", "prov-a", "claude-3", t0),
            completed_event(
                "req-1",
                "prov-a",
                "claude-3",
                t0 + second * 4,
                "0.1000000000000000000000001",
            ),
            received_event("req-2", "laptop", "prov-a", "gpt-4", t0 + second),
            completed_event(
                "req-2",
                "prov-a",
                "gpt-4",
                t0 + second * 5,
                "0.2000000000000000000000002",
            ),
            received_event("req-3", "phone", "prov-b", "claude-3", t0 + second * 2),
            failed_event("req-3", Some("prov-b"), t0 + second * 6),
            failed_event("orphan-1", None, t0 + second * 3),
            received_event(
                "old-1",
                "laptop",
                "prov-old",
                "claude-3",
                t0.saturating_sub(time::Duration::seconds(7200)),
            ),
            completed_event(
                "old-1",
                "prov-old",
                "claude-3",
                t0.saturating_sub(time::Duration::seconds(7190)),
                "0.5",
            ),
        ];
        for event in &events {
            bus.emit(event);
        }
        let closer = std::sync::Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close joined");
        Ledger {
            _dir: dir,
            path,
            t0,
        }
    }

    fn app_for(path: &std::path::Path) -> axum::Router {
        llm_proxy_api::UsageApi::new(path.to_owned(), ADMIN_TOKEN).router()
    }

    async fn get_json(app: axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::empty()).expect("request builds"))
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json = serde_json::from_slice(&bytes).expect("body is JSON");
        (status, json)
    }

    #[tokio::test]
    async fn usage_routes_refuse_missing_and_wrong_tokens_without_echoing_them() {
        let ledger = seed().await;
        for path in ["/usage/summary", "/usage/requests", "/usage/keys"] {
            let (status, body) = get_json(app_for(&ledger.path), path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} without token");
            assert_eq!(
                body["error"]["message"], "missing or invalid admin token",
                "uniform 401 body for {path}"
            );

            let (status, body_text) = {
                let (status, body) =
                    get_json(app_for(&ledger.path), path, Some("wrong-token")).await;
                (status, body.to_string())
            };
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} with wrong token");
            assert!(
                !body_text.contains("wrong-token") && !body_text.contains(ADMIN_TOKEN),
                "the 401 body must not echo any presented or configured token: {body_text}"
            );

            let (status, _) = get_json(app_for(&ledger.path), path, Some(ADMIN_TOKEN)).await;
            assert_eq!(status, StatusCode::OK, "{path} with the admin token");
        }
    }

    #[tokio::test]
    async fn summary_groups_by_provider_with_exact_decimal_costs() {
        let ledger = seed().await;
        let (status, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?group_by=provider",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let groups = body["groups"].as_array().expect("groups array");
        let find = |key: &str| {
            groups
                .iter()
                .find(|g| g["key"] == key)
                .unwrap_or_else(|| panic!("group {key} missing: {groups:?}"))
        };

        let prov_a = find("prov-a");
        assert_eq!(prov_a["requests"], 2);
        assert_eq!(prov_a["errors"], 0, "both prov-a requests completed");
        assert_eq!(
            prov_a["tokens"]["input"], 200,
            "100 input tokens per request"
        );
        assert_eq!(prov_a["tokens"]["output"], 100);
        assert_eq!(prov_a["tokens"]["reasoning"], 14);
        assert_eq!(prov_a["tokens"]["cache_read"], 400);
        assert_eq!(prov_a["tokens"]["cache_creation"], 60);
        assert_eq!(
            prov_a["cost"]["total"], "0.3000000000000000000000003",
            "cost sums must be exact decimals — a float sum of these two values \
             yields 0.30000000000000004"
        );
        assert_eq!(
            prov_a["cost"]["input"], "0.3000000000000000000000003",
            "input-cost column sums exactly too"
        );

        let prov_b = find("prov-b");
        assert_eq!(prov_b["requests"], 1);
        assert_eq!(prov_b["errors"], 1);

        let null_group = groups
            .iter()
            .find(|g| g["key"].is_null())
            .expect("failure-only rows group under a null provider");
        assert_eq!(null_group["requests"], 1);
        assert_eq!(null_group["errors"], 1);

        assert_eq!(body["group_by"], "provider");
    }

    #[tokio::test]
    async fn summary_groups_by_caller_and_model() {
        let ledger = seed().await;
        let (_, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?group_by=caller",
            Some(ADMIN_TOKEN),
        )
        .await;
        let groups = &body["groups"];
        let laptop = groups
            .as_array()
            .expect("groups")
            .iter()
            .find(|g| g["key"] == "laptop")
            .expect("laptop group");
        assert_eq!(
            laptop["requests"], 3,
            "req-1, req-2 and the two-hour-old row"
        );
        assert_eq!(laptop["errors"], 0);

        let (_, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?group_by=model",
            Some(ADMIN_TOKEN),
        )
        .await;
        let model_groups = body["groups"].as_array().expect("groups");
        let claude = model_groups
            .iter()
            .find(|g| g["key"] == "claude-3")
            .expect("claude-3 group");
        assert_eq!(claude["requests"], 3, "req-1, req-3 and old-1");
        let gpt = model_groups
            .iter()
            .find(|g| g["key"] == "gpt-4")
            .expect("gpt-4 group");
        assert_eq!(gpt["requests"], 1);
    }

    #[tokio::test]
    async fn summary_window_excludes_rows_older_than_it() {
        let ledger = seed().await;
        let (_, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?window=1h",
            Some(ADMIN_TOKEN),
        )
        .await;
        let groups = body["groups"].as_array().expect("groups");
        assert!(
            groups.iter().all(|g| g["key"] != "prov-old"),
            "the two-hour-old row must fall outside a 1h window: {groups:?}"
        );
        let (_, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?window=4h",
            Some(ADMIN_TOKEN),
        )
        .await;
        let groups = body["groups"].as_array().expect("groups");
        assert!(
            groups.iter().any(|g| g["key"] == "prov-old"),
            "a 4h window covers the two-hour-old row"
        );

        let (status, _) = get_json(
            app_for(&ledger.path),
            "/usage/summary?window=7d",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "\"7d\" must be an accepted window");
    }

    #[tokio::test]
    async fn summary_rejects_bad_window_and_group_by() {
        let ledger = seed().await;
        let (status, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?window=abc",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].to_string().contains("window"));

        let (status, _) = get_json(
            app_for(&ledger.path),
            "/usage/summary?window=0s",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "zero window is not a window"
        );

        let (status, body) = get_json(
            app_for(&ledger.path),
            "/usage/summary?group_by=team",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].to_string().contains("group_by"));
    }

    #[tokio::test]
    async fn requests_pages_newest_first_with_cursor_and_provider_filter() {
        let ledger = seed().await;
        let (status, page1) = get_json(
            app_for(&ledger.path),
            "/usage/requests?limit=2",
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows1 = page1["rows"].as_array().expect("rows");
        assert_eq!(rows1.len(), 2, "limit is honored");
        let first_id = rows1[0]["request_id"].as_str().expect("id");
        assert_eq!(
            first_id, "orphan-1",
            "newest first: orphan-1 completed latest among the seeded rows"
        );
        let cursor = page1["next_cursor"].as_str().expect("cursor present");
        assert_eq!(rows1[1]["request_id"], "req-3");

        let (status, page2) = get_json(
            app_for(&ledger.path),
            &format!("/usage/requests?limit=10&cursor={cursor}"),
            Some(ADMIN_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows2 = page2["rows"].as_array().expect("rows");
        assert!(
            rows2
                .iter()
                .all(|r| r["request_id"] != "orphan-1" && r["request_id"] != "req-3"),
            "cursor paging must not repeat rows: {rows2:?}"
        );

        let (_, filtered) = get_json(
            app_for(&ledger.path),
            "/usage/requests?provider=prov-a",
            Some(ADMIN_TOKEN),
        )
        .await;
        let filtered_rows = filtered["rows"].as_array().expect("rows");
        assert!(
            !filtered_rows.is_empty() && filtered_rows.iter().all(|r| r["provider"] == "prov-a"),
            "provider filter: {filtered_rows:?}"
        );
    }

    #[tokio::test]
    async fn requests_honors_since_and_until_bounds() {
        let ledger = seed().await;
        let until = (ledger.t0 + std::time::Duration::from_secs(3))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339");
        let (_, body) = get_json(
            app_for(&ledger.path),
            &format!("/usage/requests?until={until}"),
            Some(ADMIN_TOKEN),
        )
        .await;
        let ids: Vec<&str> = body["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|r| r["request_id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["req-3", "req-2", "req-1", "old-1"],
            "until is exclusive on the ledger time axis (orphan-1 at t0+3s is out)"
        );

        let since = (ledger.t0 + std::time::Duration::from_secs(1))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339");
        let (_, body) = get_json(
            app_for(&ledger.path),
            &format!("/usage/requests?since={since}"),
            Some(ADMIN_TOKEN),
        )
        .await;
        let ids: Vec<&str> = body["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|r| r["request_id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["orphan-1", "req-3", "req-2"],
            "since is inclusive"
        );
    }

    #[tokio::test]
    async fn requests_rejects_invalid_parameters() {
        let ledger = seed().await;
        let cases = [
            "/usage/requests?limit=0",
            "/usage/requests?limit=1001",
            "/usage/requests?limit=-5",
            "/usage/requests?since=not-a-time",
            "/usage/requests?until=not-a-time",
            "/usage/requests?cursor=garbage",
            "/usage/requests?cursor=123:",
            "/usage/requests?since=2026-01-02T03:04:05Z&until=2026-01-01T00:00:00Z",
        ];
        for uri in cases {
            let (status, body) = get_json(app_for(&ledger.path), uri, Some(ADMIN_TOKEN)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        }
    }

    #[tokio::test]
    async fn requests_rows_carry_digests_but_never_bodies() {
        let ledger = seed().await;
        let (_, body) = get_json(app_for(&ledger.path), "/usage/requests", Some(ADMIN_TOKEN)).await;
        let text = body.to_string();
        assert!(
            text.contains("sha256-digest-"),
            "rows expose the body digest for correlation"
        );
        for row in body["rows"].as_array().expect("rows") {
            let keys: Vec<&str> = row
                .as_object()
                .expect("row object")
                .keys()
                .map(String::as_str)
                .collect();
            assert!(
                !keys
                    .iter()
                    .any(|k| *k == "body" || *k == "request_body" || *k == "message"),
                "raw rows must never carry request bodies or sanitized messages: {keys:?}"
            );
        }
    }

    #[tokio::test]
    async fn keys_lists_callers_with_counts_and_last_seen() {
        let ledger = seed().await;
        let (status, body) =
            get_json(app_for(&ledger.path), "/usage/keys", Some(ADMIN_TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        let callers = body["callers"].as_array().expect("callers");
        let find = |name: &str| {
            callers
                .iter()
                .find(|c| c["caller"] == name)
                .unwrap_or_else(|| panic!("caller {name} missing: {callers:?}"))
        };
        let laptop = find("laptop");
        assert_eq!(laptop["requests"], 3, "req-1, req-2, old-1");
        assert_eq!(laptop["errors"], 0);
        let phone = find("phone");
        assert_eq!(phone["requests"], 1);
        assert_eq!(phone["errors"], 1);
        assert!(laptop["last_seen_ms"].is_u64(), "last_seen_ms is populated");

        let dated: Vec<i64> = callers
            .iter()
            .filter_map(|c| c["last_seen_ms"].as_i64())
            .collect();
        assert!(
            !dated.is_empty(),
            "callers with ledger activity must carry last_seen_ms: {callers:?}"
        );
        assert_eq!(
            dated,
            {
                let mut sorted = dated.clone();
                sorted.sort_by(|a, b| b.cmp(a));
                sorted
            },
            "callers appear most-recently-seen first"
        );
        let order: Vec<&str> = callers
            .iter()
            .map(|c| c["caller"].as_str().unwrap_or("<null>"))
            .collect();
        assert_eq!(
            order,
            vec!["<null>", "phone", "laptop"],
            "caller ordering follows last-seen timestamps, null caller included"
        );
    }

    #[tokio::test]
    async fn summary_serves_while_the_ledger_writer_is_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        let bus = std::sync::Arc::new(
            SqliteEventBus::open(
                &path,
                llm_proxy_storage::RetentionPolicy {
                    max_age: None,
                    max_rows: None,
                    prune_every: std::time::Duration::from_secs(3600),
                },
            )
            .expect("open ledger"),
        );
        bus.emit(&received_event(
            "live-1",
            "laptop",
            "prov-live",
            "claude-3",
            OffsetDateTime::now_utc(),
        ));

        let mut seen = false;
        for _ in 0..200 {
            let (status, body) = get_json(
                app_for(&path),
                "/usage/summary?group_by=provider",
                Some(ADMIN_TOKEN),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "query against live writer");
            let found = body["groups"]
                .as_array()
                .is_some_and(|g| g.iter().any(|x| x["key"] == "prov-live"));
            if found {
                seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            seen,
            "the row must become visible while the writer is active"
        );
        let closer = std::sync::Arc::clone(&bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close joined");
    }
}
