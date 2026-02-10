use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing::info;

/// Listen for OS signals and dispatch to the appropriate channel.
///
/// - SIGTERM / SIGINT -> sends on `shutdown_tx`
/// - SIGHUP -> sends on `reload_tx`
///
/// This task runs until a shutdown signal is received.
pub async fn signal_listener(shutdown_tx: watch::Sender<bool>, reload_tx: watch::Sender<()>) {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");
    let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP handler");

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, initiating graceful shutdown");
                let _ = shutdown_tx.send(true);
                return;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, initiating graceful shutdown");
                let _ = shutdown_tx.send(true);
                return;
            }
            _ = sighup.recv() => {
                info!("received SIGHUP, triggering config reload");
                let _ = reload_tx.send(());
            }
        }
    }
}
