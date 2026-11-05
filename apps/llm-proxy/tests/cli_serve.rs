//! Integration tests for the `serve` command: pre-flight config validation
//! (the full extension matrix lives in `src/config_validation.rs`) and the
//! shutdown deadline, driven against the real binary.

#[cfg(test)]
mod tests {

    use llm_proxy_app::config_validation::validate_toml_extension;

    #[test]
    fn serve_rejects_non_toml_config_with_unsupported_message() {
        let result = validate_toml_extension(std::path::Path::new("config.json"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported"),
            "expected 'unsupported' in error: {msg}"
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod shutdown_deadline {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    const WAIT_LIMIT: Duration = Duration::from_secs(15);

    struct RunningServer {
        child: Child,
        addr: SocketAddr,
        _dir: tempfile::TempDir,
    }

    fn write_config(dir: &Path, addr: SocketAddr, upstream: SocketAddr) -> std::path::PathBuf {
        let providers = dir.join("providers");
        std::fs::create_dir(&providers).unwrap();
        std::fs::write(
            providers.join("upstream.toml"),
            format!(
                r#"[provider]
name = "upstream"
api_key = "test-upstream-key-not-real"
auth_style = "bearer"
[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "http://{upstream}/chat/completions"
[provider.routes]
chat_completions = "chat"
"#
            ),
        )
        .unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"[server]
bind = "{addr}"
request_timeout = "60s"
shutdown_timeout = "1s"
log_level = "warn"
hot_reload = false
rate_limit_rpm = 0
trust_forwarded_headers = false
server_name = "llm-proxy"
[auth]
mode = "none"
[storage]
path = "{storage}"
"#,
                storage = dir.join("usage.db").display(),
            ),
        )
        .unwrap();
        config
    }

    impl RunningServer {
        /// Starts the binary with a one-second drain deadline and returns once
        /// it accepts connections.
        fn start(upstream: SocketAddr) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let addr = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let config = write_config(dir.path(), addr, upstream);

            let child = Command::new(env!("CARGO_BIN_EXE_llm-proxy"))
                .args(["serve", "--config"])
                .arg(&config)
                .env("HOME", dir.path())
                .env("NO_PROXY", "127.0.0.1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut server = Self {
                child,
                addr,
                _dir: dir,
            };
            server.wait_until_listening();
            server
        }

        fn wait_until_listening(&mut self) {
            let started = Instant::now();
            while TcpStream::connect(self.addr).is_err() {
                let exited = self.child.try_wait().unwrap();
                assert!(exited.is_none(), "server exited during startup: {exited:?}");
                assert!(started.elapsed() < WAIT_LIMIT, "server never started");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn send_sigterm(&self) {
            let status = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }

        fn wait_for_exit(&mut self) -> ExitStatus {
            let started = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(started.elapsed() < WAIT_LIMIT, "server never exited");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for RunningServer {
        fn drop(&mut self) {
            if let Err(error) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
                eprintln!("failed to stop the test server: {error}");
            }
        }
    }

    fn silent_upstream() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").unwrap()
    }

    /// Returns the upstream side of the proxied request, which never gets a reply.
    fn accept_within_limit(upstream: &TcpListener) -> TcpStream {
        let started = Instant::now();
        loop {
            match upstream.accept() {
                Ok((stream, _)) => return stream,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        started.elapsed() < WAIT_LIMIT,
                        "request never reached upstream"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("upstream accept failed: {error}"),
            }
        }
    }

    #[test]
    fn server_outlives_its_deadline_until_signalled_then_exits_cleanly() {
        let upstream = silent_upstream();
        let mut server = RunningServer::start(upstream.local_addr().unwrap());

        std::thread::sleep(Duration::from_secs(2));

        let exited = server.child.try_wait().unwrap();
        assert!(exited.is_none(), "server exited on its own: {exited:?}");
        let mut health = TcpStream::connect(server.addr).unwrap();
        health
            .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        health.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "health reply: {reply}");

        server.send_sigterm();
        assert!(server.wait_for_exit().success());
    }

    #[test]
    fn deadline_ends_a_drain_that_a_stuck_request_would_hold_open() {
        let upstream = silent_upstream();
        upstream.set_nonblocking(true).unwrap();
        let mut server = RunningServer::start(upstream.local_addr().unwrap());

        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let mut client = TcpStream::connect(server.addr).unwrap();
        write!(
            client,
            "POST /providers/upstream/v1/chat/completions HTTP/1.1\r\nHost: t\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let _held_open = accept_within_limit(&upstream);

        server.send_sigterm();
        assert_eq!(server.wait_for_exit().code(), Some(1));
    }
}
