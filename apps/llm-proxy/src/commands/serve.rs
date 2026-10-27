use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::serve::{Listener, ListenerExt};
use llm_proxy_core::LogFormat;
use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
use llm_proxy_server::{admin_router, build_router, shutdown_signal};
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
    init_tracing(&LogFormat::from_env());

    if background && !daemonize {
        let path = resolve_config(config_path.as_deref());
        validate_toml_extension(&path)?;
        return spawn_daemon(config_path.as_deref(), port_override);
    }

    let path = resolve_config(config_path.as_deref());

    validate_toml_extension(&path)?;

    let adapter_registry = ProviderAdapterRegistry::builtin();
    let proxy_client = ProxyClient::try_new().with_context(
        || "building proxy HTTP client (check HTTP_PROXY/HTTPS_PROXY and TLS config)",
    )?;

    let state = load_toml_state(&path, adapter_registry, proxy_client, port_override)?;

    let admin = bind_admin_listener(&state).await?;

    if let Err(first_err) = write_pid() {
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
        write_pid().with_context(|| {
            format!(
                "failed to create PID file after removing stale entry: {}",
                first_err
            )
        })?;
    }

    let bind_addr = state.bind_address();
    let shutdown_deadline = state.shutdown_timeout();
    let event_bus = std::sync::Arc::clone(state.event_bus());
    let app = build_router(state);

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

    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    let admin_serve = async {
        if let Some((listener, router)) = admin {
            if let Err(error) = axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(shutdown_signal())
                .await
            {
                tracing::error!(%error, "admin usage listener failed");
            }
        }
    };

    let result = serve_with_deadline(serve.into_future(), admin_serve, shutdown_deadline).await;

    if let Err(e) = remove_pid() {
        tracing::warn!(error = %e, "failed to remove PID file during shutdown");
    }

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

async fn serve_with_deadline<F, A>(
    serve: F,
    admin_serve: A,
    shutdown_deadline: std::time::Duration,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
    A: std::future::Future<Output = ()>,
{
    if shutdown_deadline.is_zero() {
        let (main, ()) = tokio::join!(serve, admin_serve);
        return main;
    }
    match tokio::time::timeout(shutdown_deadline, async {
        tokio::join!(serve, admin_serve)
    })
    .await
    {
        Ok((main, ())) => main,
        Err(_) => {
            tracing::error!(
                deadline = ?shutdown_deadline,
                "graceful-shutdown deadline elapsed; forcing exit with in-flight connections dropped"
            );
            if let Err(e) = remove_pid() {
                tracing::warn!(error = %e, "failed to remove PID file during shutdown");
            }
            std::process::exit(1);
        }
    }
}

async fn bind_admin_listener(
    state: &llm_proxy_server::AppState,
) -> Result<Option<(TcpListener, axum::Router)>> {
    let Some(posture) = state.app_config().admin_posture() else {
        return Ok(None);
    };
    if !state.app_config().server.dedicated_admin_listener() {
        tracing::info!(
            posture = posture.name(),
            "admin usage API served on the main listener only; no dedicated socket"
        );
        return Ok(None);
    }
    let Some(addr) = state.app_config().server.effective_admin_bind() else {
        bail!("admin API enabled but no admin bind could be derived");
    };
    let Some(router) = admin_router(state) else {
        bail!("admin API enabled but no /usage router could be built");
    };
    let listener = bind_listener(addr).await?;
    info!(
        addr = %addr,
        posture = posture.name(),
        "admin usage API listening (read-only, behind the admin token)"
    );
    Ok(Some((listener, router)))
}

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
        socket
            .set_nonblocking(true)
            .with_context(|| "setting nonblocking on listener")?;
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

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        let log_path = config_dir().join("llm-proxy.log");
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

    if let Err(e) = write_pid_value(pid) {
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

    std::thread::sleep(std::time::Duration::from_millis(100));
    match child.try_wait() {
        Ok(Some(status)) => {
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
        Ok(None) => {}
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
