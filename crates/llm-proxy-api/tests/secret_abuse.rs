//! Secret-abuse checks for the `/usage` surface: no credential reaches the
//! captured tracing output or the raw ledger bytes, and every negative scan
//! first asserts seeded ledger content is findable.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived, SqliteEventBus};
    use tower::ServiceExt;

    const ADMIN_TOKEN: &str = "prx6-api-admin-token-0a3f5e91";
    const WRONG_TOKEN: &str = "prx6-api-attacker-guess-2c4d";
    const CLIENT_TOKEN: &str = "prx6-api-client-token-7e8f";
    const CALLER_MARKER: &str = "prx6-seeded-caller-marker-4451";

    async fn seeded_ledger(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("usage.db");
        let bus = SqliteEventBus::open(
            &path,
            llm_proxy_storage::RetentionPolicy {
                max_age: None,
                max_rows: None,
                prune_every: Duration::from_secs(3600),
            },
        )
        .expect("ledger opens");
        bus.emit(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: "prx6-seed".to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: CALLER_MARKER.to_owned(),
            provider: "prov-seed".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: llm_proxy_protocol::core::ModelRef {
                requested: "claude-3".to_owned(),
                upstream: None,
            },
            streaming: false,
            body_hash: "prx6-seed-digest".to_owned(),
        }));
        let closer = Arc::new(bus);
        tokio::task::spawn_blocking(move || closer.close())
            .await
            .expect("close joins");
        path
    }

    fn usage_app(path: &Path) -> Router {
        llm_proxy_api::UsageApi::new(path.to_owned(), ADMIN_TOKEN).router()
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::empty()).expect("request builds")
    }

    async fn drive_all_outcomes(path: &Path) {
        for uri in ["/usage/summary", "/usage/requests", "/usage/keys"] {
            for token in [
                Some(ADMIN_TOKEN),
                Some(WRONG_TOKEN),
                Some(CLIENT_TOKEN),
                None,
            ] {
                let (status, body) = send(usage_app(path).clone(), get(uri, token)).await;
                let expected = if token == Some(ADMIN_TOKEN) {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                };
                assert_eq!(status, expected, "{uri} with {token:?}: {body}");
            }
        }
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, String) {
        let response = app.oneshot(request).await.expect("oneshot completes");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

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

    struct TraceCapture {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    static CAPTURE_BUFFER: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    static CAPTURE_INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

    impl TraceCapture {
        fn install() -> Self {
            use tracing_subscriber::layer::SubscriberExt;
            let buffer = CAPTURE_BUFFER.get_or_init(|| Arc::new(Mutex::new(Vec::<u8>::new())));
            CAPTURE_INSTALLED.get_or_init(|| {
                let layer = tracing_subscriber::fmt::layer()
                    .with_writer(BufferWriter(Arc::clone(buffer)))
                    .with_ansi(false);
                tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
                    .expect("the global capture subscriber installs exactly once");
            });
            Self {
                buffer: Arc::clone(buffer),
            }
        }

        fn text(&self) -> String {
            let bytes = self.buffer.lock().expect("buffer lock").clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    #[derive(Clone)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriterGuard<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            BufferWriterGuard(self.0.lock().expect("buffer lock"))
        }
    }

    struct BufferWriterGuard<'a>(MutexGuard<'a, Vec<u8>>);

    impl std::io::Write for BufferWriterGuard<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn no_credential_ever_lands_in_the_raw_ledger_file_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = seeded_ledger(&dir).await;
        drive_all_outcomes(&path).await;

        let bytes = ledger_bytes(&path);
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(CALLER_MARKER) && text.contains("prx6-seed-digest"),
            "control: seeded persisted values must be findable in the ledger bytes"
        );
        for secret in [ADMIN_TOKEN, WRONG_TOKEN, CLIENT_TOKEN] {
            assert!(
                !text.contains(secret),
                "credential {secret} leaked into the raw ledger file bytes"
            );
        }
    }

    #[tokio::test]
    async fn admin_token_never_appears_in_captured_usage_tracing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = seeded_ledger(&dir).await;

        let capture = TraceCapture::install();
        drive_all_outcomes(&path).await;
        let captured = capture.text();
        assert!(
            captured.contains("started processing request"),
            "control: usage request traffic must land in the capture, got: {captured}"
        );
        for secret in [ADMIN_TOKEN, WRONG_TOKEN, CLIENT_TOKEN] {
            assert!(
                !captured.contains(secret),
                "credential {secret} leaked into tracing output: {captured}"
            );
        }
    }
}
