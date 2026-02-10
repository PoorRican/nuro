mod admin;
mod config;
mod shutdown;
mod telemetry;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::{watch, RwLock};
use tracing::{error, info};

/// Neuromancer daemon — deterministic orchestrator for rig-powered sub-agents.
#[derive(Parser, Debug)]
#[command(name = "neuromancerd", version, about)]
struct Cli {
    /// Config file path.
    #[arg(short, long, default_value = "neuromancer.toml")]
    config: PathBuf,

    /// Increase log verbosity (debug level).
    #[arg(short, long)]
    verbose: bool,

    /// Validate config and exit.
    #[arg(long)]
    validate: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // -----------------------------------------------------------------------
    // 1. Load and validate config
    // -----------------------------------------------------------------------
    let initial_config = config::load_config(&cli.config)?;
    config::validate_config(&initial_config)?;

    if cli.validate {
        println!("config is valid");
        return Ok(());
    }

    // -----------------------------------------------------------------------
    // 2. Initialize tracing / OTEL
    // -----------------------------------------------------------------------
    let _telemetry_guard = telemetry::init_telemetry(&initial_config.otel, cli.verbose)?;

    info!(
        instance_id = %initial_config.global.instance_id,
        "neuromancerd starting"
    );

    // -----------------------------------------------------------------------
    // 3. Set up shared state and config watch
    // -----------------------------------------------------------------------
    let config = Arc::new(RwLock::new(initial_config.clone()));

    // Config hot-reload watcher
    let (_watcher, mut config_rx) = config::spawn_config_watcher(&cli.config)?;

    // Channels for signals
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (reload_tx, _reload_rx) = watch::channel(());

    // -----------------------------------------------------------------------
    // 4. (Stub) Open databases, init secrets broker, MCP pool, agent registry,
    //    triggers. These will be integrated as sibling crates are completed.
    // -----------------------------------------------------------------------
    info!("stub: databases, secrets broker, MCP pool, agent registry, triggers not yet initialized");

    // -----------------------------------------------------------------------
    // 5. Start admin API server
    // -----------------------------------------------------------------------
    let admin_state = admin::AppState {
        config: config.clone(),
        start_time: Instant::now(),
        config_reload_tx: reload_tx.clone(),
        submitted_tasks: Arc::new(RwLock::new(HashMap::new())),
    };

    let admin_router = admin::admin_router(admin_state);
    let bind_addr = initial_config.admin_api.bind_addr.clone();

    let listener = TcpListener::bind(&bind_addr).await?;
    info!(bind = %bind_addr, "admin API listening");

    // Spawn the admin server as a background task.
    let admin_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, admin_router)
            .with_graceful_shutdown(async move {
                shutdown_rx.changed().await.ok();
            })
            .await
        {
            error!("admin API server error: {e}");
        }
    });

    // -----------------------------------------------------------------------
    // 6. Spawn signal handler
    // -----------------------------------------------------------------------
    tokio::spawn(shutdown::signal_listener(shutdown_tx.clone(), reload_tx));

    // -----------------------------------------------------------------------
    // 7. Main orchestrator loop (stub — select on shutdown + config reload)
    // -----------------------------------------------------------------------
    let mut shutdown_watch = shutdown_tx.subscribe();

    info!("entering main loop");
    loop {
        tokio::select! {
            _ = shutdown_watch.changed() => {
                if *shutdown_watch.borrow() {
                    info!("shutdown signal received, beginning graceful shutdown");
                    break;
                }
            }
            _ = config_rx.changed() => {
                let new_config = config_rx.borrow().clone();
                info!("applying hot-reloaded config");
                let mut cfg = config.write().await;
                *cfg = (*new_config).clone();
                info!("config updated");
            }
        }
    }

    // -----------------------------------------------------------------------
    // 8. Graceful shutdown sequence (§20.3)
    // -----------------------------------------------------------------------
    info!("graceful shutdown: stopping trigger sources (stub)");

    // Drain in-flight tasks with timeout
    info!("graceful shutdown: draining in-flight tasks (stub, 30s timeout)");
    let drain_timeout = Duration::from_secs(30);
    let _ = tokio::time::timeout(drain_timeout, async {
        // Stub: would wait for task queue to drain
    })
    .await;

    // Wait for admin server to finish
    info!("graceful shutdown: stopping admin API");
    let _ = admin_handle.await;

    // Flush OTEL spans (handled by TelemetryGuard drop, but explicit for clarity)
    info!("graceful shutdown: flushing OTEL spans");
    _telemetry_guard.flush();

    info!("graceful shutdown: closing connections (stub)");
    info!("neuromancerd stopped");

    Ok(())
}
