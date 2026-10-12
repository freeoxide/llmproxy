//! `serve` command implementation.
//!
//! Starts the proxy server in foreground or background (daemon) mode.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::serve::{Listener, ListenerExt};
use llm_proxy_core::LogFormat;
use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
use llm_proxy_server::{build_router, shutdown_signal};
use tokio::net::TcpListener;
use tracing::info;

use crate::config_validation::validate_toml_extension;
use crate::paths::{config_dir, resolve_config};
use crate::pid::{is_process_running, read_pid, remove_pid, write_pid, write_pid_value};
use crate::state::{init_tracing, load_toml_state};

/// Run the `serve` command.
///
/// Requires TOML configuration.
pub async fn cmd_serve(
    config_path: Option<PathBuf>,
    port_override: Option<u16>,
    background: bool,
    daemonize: bool,
) -> Result<()> {
    // Initialize tracing before the daemonization check so that the parent
    // process can log during spawn_daemon(). The daemon child reinitializes
    // its own subscriber; calling init_tracing twice is safe (the second
    // call is a no-op since the global subscriber is already set).
    init_tracing(&LogFormat::from_env());

    // If background mode requested, spawn self as child with --daemonize.
    // Run the cheap, pure config checks in the parent first so a clearly-bad
    // --config (wrong extension / unresolvable path) produces a normal error
    // rather than a spawned-then-immediately-died daemon. Full config loading
    // (HTTP client build, TOML parse) stays in the child.
    if background && !daemonize {
        let path = resolve_config(config_path.as_deref());
        validate_toml_extension(&path)?;
        return spawn_daemon(config_path.as_deref(), port_override);
    }

    let path = resolve_config(config_path.as_deref());

    validate_toml_extension(&path)?;

    // Build shared infrastructure.
    let adapter_registry = ProviderAdapterRegistry::builtin();
    // Build the HTTP client fallibly. `reqwest::Client::builder().build()` can
    // fail at startup on an invalid `HTTP_PROXY`/`HTTPS_PROXY` env value or a
    // TLS-init failure; using `try_new` (audit GAP-LOW-13) lets us surface that
    // as a contextualized error instead of panicking the daemon.
    let proxy_client = ProxyClient::try_new().with_context(
        || "building proxy HTTP client (check HTTP_PROXY/HTTPS_PROXY and TLS config)",
    )?;

    // Load TOML config and build state before writing the PID file. This
    // ensures a bad config does not leave a stale PID file on disk.
    let state = load_toml_state(&path, adapter_registry, proxy_client, port_override)?;

    // Attempt atomic PID file creation. If it fails because the file already
    // exists, check whether the existing owner is still alive before removing
    // the stale file and retrying. This closes the TOCTOU window between the
    // stale-check and the write.
    if let Err(first_err) = write_pid() {
        // Atomic create_new failed -- file exists. Check if the owner is stale.
        if let Some(existing_pid) = read_pid()? {
            if is_process_running(existing_pid) {
                bail!(
                    "server already running with PID {} (PID file: {})",
                    existing_pid,
                    crate::paths::pid_file_path().display()
                );
            }
            info!("stale PID file found (PID {}), cleaning up", existing_pid);
        }
        remove_pid()?;
        // Retry -- this time the file is gone so create_new should succeed.
        write_pid().with_context(|| {
            format!(
                "failed to create PID file after removing stale entry: {}",
                first_err
            )
        })?;
    }

    // Ensure PID file is cleaned up on shutdown. The two removal sites below
    // are mutually exclusive along any path (the deadline branch is followed
    // by process::exit), so remove_pid() runs exactly once per shutdown.
    let bind_addr = state.bind_address();
    let shutdown_deadline = state.shutdown_timeout();
    // Hold the event bus so the usage ledger can be flushed after the server
    // stops. The router (which owns the state) keeps its own Arc; closing is
    // watch-signalled, not sender-drop-signalled, so those clones do not
    // hold the writer open.
    let event_bus = std::sync::Arc::clone(state.event_bus());
    let app = build_router(state);

    // `tap_io` runs on every accepted connection (audit MEDIUM-2 option A).
    // `TCP_NODELAY` is a per-connection socket option that is *not* inherited
    // from the listening socket, so it must be applied to each accepted
    // `TcpStream` here. Disabling Nagle flushes small SSE chunks to the client
    // without the ~40ms coalescing delay, which matters for streaming token
    // output. `SO_KEEPALIVE` is set on the listener in `bind_listener`.
    let listener = bind_listener(bind_addr).await?.tap_io(|tcp_stream| {
        if let Err(error) = tcp_stream.set_nodelay(true) {
            tracing::trace!(%error, "failed to set TCP_NODELAY on accepted connection");
        }
    });
    info!(
        addr = %listener.local_addr()?,
        version = env!("CARGO_PKG_VERSION"),
        "llm-proxy listening"
    );

    // Serve with graceful shutdown bounded by a hard deadline.
    //
    // axum 0.8's `with_graceful_shutdown` waits indefinitely for in-flight
    // connections to drain. Because the proxy serves long-lived streaming
    // responses an upstream can hold open arbitrarily, one stuck stream could
    // otherwise pin the process forever on SIGTERM/SIGINT. We wrap the serve
    // future in `tokio::time::timeout`: once the deadline elapses after the
    // signal, stuck connections are dropped and we force-exit so an
    // orchestrator (Kubernetes/LB) can recycle the pod. `shutdown_timeout == 0`
    // disables the deadline (drain forever) for operators who want that.
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    let result = if shutdown_deadline.is_zero() {
        serve.await
    } else {
        match tokio::time::timeout(shutdown_deadline, serve).await {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(
                    deadline = ?shutdown_deadline,
                    "graceful-shutdown deadline elapsed; forcing exit with in-flight connections dropped"
                );
                // Run PID cleanup best-effort, then exit. The deadline exists
                // precisely to escape stuck connections, so we do not wait for
                // them; remove_pid is fast, synchronous filesystem I/O.
                if let Err(e) = remove_pid() {
                    tracing::warn!(error = %e, "failed to remove PID file during shutdown");
                }
                std::process::exit(1);
            }
        }
    };

    if let Err(e) = remove_pid() {
        tracing::warn!(error = %e, "failed to remove PID file during shutdown");
    }

    // Flush the usage ledger now that the graceful drain has finished and no
    // new events can arrive: the writer drains its channel and exits. For
    // the default NoopBus this is a no-op. close() blocks (bounded), so it
    // runs on the blocking pool. (The hard-deadline branch above force-exits
    // without flushing; queued events are lost there by design.)
    if let Err(close_err) = tokio::task::spawn_blocking(move || event_bus.close()).await {
        tracing::warn!(error = %close_err, "usage-ledger close task failed to join");
    }

    match result {
        Ok(()) => {
            info!("server stopped cleanly");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "server error");
            Err(anyhow::anyhow!("server error: {}", e))
        }
    }
}

/// Bind the listening TCP socket with connection-hardening options.
///
/// On Unix the socket is created via `socket2` so we can enable `SO_KEEPALIVE`
/// with a bounded idle interval: half-open or stalled peers (a dropped VPN, a
/// NAT timeout, an idle slowloris connection) are probed and reaped instead of
/// pinning a connection forever. Accepted connections inherit the listener's
/// keepalive setting on Linux and macOS.
///
/// `SO_REUSEADDR` is set so a quick restart after a crash can rebind while the
/// previous socket is in `TIME_WAIT`.
///
/// # Slowloris note
///
/// TCP keepalive does not by itself stop a client that drip-feeds request
/// *headers* one byte at a time: `TimeoutLayer` only bounds the fully-buffered
/// handler body, and axum 0.8 exposes no per-connection read-header timeout.
/// For internet-facing deployments, place this proxy behind a reverse proxy
/// (nginx, envoy, a cloud LB) that imposes a read-header timeout and
/// connection cap. For the typical loopback / developer-facing deployment the
/// risk is negligible.
async fn bind_listener(bind_addr: std::net::SocketAddr) -> Result<TcpListener> {
    #[cfg(unix)]
    {
        use socket2::{Domain, Protocol, Socket, TcpKeepalive};
        let domain = Domain::for_address(bind_addr);
        let socket = Socket::new(domain, socket2::Type::STREAM, Some(Protocol::TCP))
            .with_context(|| format!("creating socket for {bind_addr}"))?;
        socket
            .set_reuse_address(true)
            .with_context(|| "setting SO_REUSEADDR")?;
        let keepalive = TcpKeepalive::new().with_time(std::time::Duration::from_secs(60));
        socket
            .set_tcp_keepalive(&keepalive)
            .with_context(|| "enabling TCP keepalive")?;
        socket
            .bind(&bind_addr.into())
            .with_context(|| format!("binding to {bind_addr}"))?;
        socket
            .listen(1024)
            .with_context(|| format!("listening on {bind_addr}"))?;
        // tokio's I/O driver requires a non-blocking socket.
        socket
            .set_nonblocking(true)
            .with_context(|| "setting nonblocking on listener")?;
        // Convert the configured socket2 socket into a tokio listener.
        let std_listener = std::net::TcpListener::from(socket);
        Ok(TcpListener::from_std(std_listener)?)
    }
    #[cfg(not(unix))]
    {
        TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("binding to {bind_addr}"))
    }
}

/// Spawn the current binary as a background daemon.
fn spawn_daemon(config_path: Option<&Path>, port_override: Option<u16>) -> Result<()> {
    let exe = std::env::current_exe().with_context(|| "resolving current executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve").arg("--daemonize");

    if let Some(p) = config_path {
        cmd.arg("--config").arg(p);
    }
    if let Some(p) = port_override {
        cmd.arg("--port").arg(p.to_string());
    }

    // Detach from terminal.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        // Redirect stdout/stderr to a log file.
        // NOTE: The log file grows unbounded. For long-running daemons, set up
        // external log rotation (e.g., logrotate on Linux, newsyslog on macOS)
        // or pipe to a log aggregation service.
        let log_path = config_dir().join("llm-proxy.log");
        // Open the log file born-private (0600) via `open_private_append`,
        // avoiding the create-then-chmod TOCTOU window where the file briefly
        // existed world-readable under the process umask. Daemon logs may
        // reference requests/config and (per GAP-MED-1) redaction can leak key
        // suffixes, so the file must never be readable by other local users.
        let log_file = crate::permissions::open_private_append(&log_path)
            .with_context(|| format!("opening log file {}", log_path.display()))?;
        cmd.stdout(
            log_file
                .try_clone()
                .with_context(|| "cloning log file handle")?,
        );
        cmd.stderr(log_file);
    }

    #[cfg(not(unix))]
    {
        println!(
            "warning: daemon log redirection is not supported on this platform; \
                   output will be lost"
        );
    }

    let mut child = cmd.spawn().with_context(|| "spawning daemon process")?;

    let pid = child.id();

    // Write the PID file before detaching so callers can reliably find it.
    //
    // A failed write here is fatal for the background path: `cmd_stop` would
    // print "no PID file found -- server not running" and be unable to stop
    // the daemon, and a second `serve --background` is only guarded by the
    // child's own create_new PID-file check (serve.rs:63) rather than this
    // parent write. Rather than hand the user an unmanageable daemon, kill
    // the just-spawned child and bail so they get a clear, recoverable error
    // (audit finding app-cli:serve-daemon-pid-write-failure-orphan).
    if let Err(e) = write_pid_value(pid) {
        // Best-effort cleanup: send SIGKILL so the child cannot continue, then
        // reap it. We do not propagate a kill/reap failure over the original
        // PID-write error -- the original cause is what the user needs to see.
        if let Err(kill_err) = child.kill() {
            tracing::error!(
                error = %kill_err,
                pid,
                "failed to kill orphaned daemon child after PID-file write failure"
            );
        }
        match child.wait() {
            Ok(status) => {
                tracing::info!(%status, "reaped daemon child after PID-file write failure");
            }
            Err(wait_err) => {
                tracing::warn!(error = %wait_err, "failed to reap daemon child");
            }
        }
        bail!(
            "daemon child (PID {pid}) was spawned but writing its PID file failed: {e}; \
             the child has been killed to avoid leaving an unmanageable daemon"
        );
    }

    // Confirm the child is still alive before reporting success.
    std::thread::sleep(std::time::Duration::from_millis(100));
    match child.try_wait() {
        Ok(Some(status)) => {
            // The child died before it could manage its own PID file (it only
            // reaches its write_pid() after load_toml_state succeeds), so remove
            // the one we wrote to avoid leaving a stale entry on disk.
            if let Err(e) = crate::pid::remove_pid() {
                tracing::warn!(error = %e, "failed to remove PID file after daemon child exited immediately");
            }
            bail!(
                "daemon child exited immediately with status {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_owned())
            );
        }
        Ok(None) => { /* child is running */ }
        Err(e) => {
            bail!("failed to check daemon child status: {}", e);
        }
    }

    println!("llm-proxy started in background (PID {pid})");
    println!("  config dir: {}", config_dir().display());
    println!(
        "  log file:   {}",
        config_dir().join("llm-proxy.log").display()
    );

    Ok(())
}
