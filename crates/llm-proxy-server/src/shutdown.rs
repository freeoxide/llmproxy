use tokio::signal;

/// Wait for SIGINT (Ctrl-C) or SIGTERM (Unix only).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        match signal::ctrl_c().await {
            Ok(()) => tracing::info!("received SIGINT (Ctrl-C)"),
            Err(error) => tracing::error!(
                %error,
                "failed to install Ctrl-C handler; beginning graceful shutdown"
            ),
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
                tracing::info!("received SIGTERM");
            }
            Err(error) => tracing::error!(
                %error,
                "failed to install SIGTERM handler; beginning graceful shutdown"
            ),
        }
    };

    #[cfg(not(unix))]
    let terminate = async {
        std::future::pending::<()>().await;
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
