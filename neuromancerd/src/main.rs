mod config;
mod orchestrator;
mod rpc;
mod shutdown;
mod telemetry;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc, watch};
use tracing::{error, info, warn};

/// Neuromancer daemon — System0 orchestrator runtime for rig-powered sub-agents.
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
    config::validate_config(&initial_config, &cli.config)?;

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
    // 4. Initialize secrets broker, MCP pool
    // -----------------------------------------------------------------------
    let secrets_broker: Option<Arc<dyn neuromancer_core::secrets::SecretsBroker>> =
        match init_secrets_broker(&initial_config).await {
            Ok(broker) => Some(broker),
            Err(err) => {
                warn!("secrets broker initialization failed (non-fatal): {err}");
                None
            }
        };

    let mcp_pool = {
        let pool = Arc::new(neuromancer_mcp::McpClientPool::new(
            initial_config.mcp_servers.clone(),
        ));
        if let Err(err) = pool.start_all().await {
            warn!("MCP pool start_all failed (non-fatal): {err}");
        } else {
            let statuses = pool.server_statuses().await;
            let running = statuses.values().filter(|s| matches!(s, neuromancer_mcp::ServerStatus::Running { .. })).count();
            info!(total = statuses.len(), running, "MCP pool initialized");
        }
        Some(pool)
    };

    // -----------------------------------------------------------------------
    // 5. Start admin API server
    // -----------------------------------------------------------------------
    let system0_runtime = Arc::new(
        orchestrator::System0Runtime::new(
            &initial_config,
            &cli.config,
            secrets_broker,
            mcp_pool.clone(),
        )
        .await
        .map_err(|err| anyhow!("failed to initialize orchestrator runtime: {err}"))?,
    );

    let rpc_state = rpc::AppState {
        start_time: Instant::now(),
        config_reload_tx: reload_tx.clone(),
        system0_runtime: Some(system0_runtime.clone()),
    };

    let rpc_router = rpc::rpc_router(rpc_state);
    let bind_addr = initial_config.admin_api.bind_addr.clone();

    let listener = TcpListener::bind(&bind_addr).await?;
    info!(bind = %bind_addr, "admin API listening");

    // Spawn the admin server as a background task.
    let rpc_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, rpc_router)
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
    // 7. Initialize trigger manager and enter main loop
    // -----------------------------------------------------------------------
    let (trigger_tx, mut trigger_rx) = mpsc::channel::<neuromancer_core::trigger::TriggerEvent>(128);
    let mut trigger_manager =
        neuromancer_triggers::TriggerManager::new(&initial_config.triggers, trigger_tx);
    if let Err(err) = trigger_manager.start().await {
        warn!("trigger manager start failed (non-fatal): {err}");
    } else {
        info!("trigger manager started");
    }

    let mut shutdown_watch = shutdown_tx.subscribe();
    let runtime_for_triggers = system0_runtime.clone();

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
            Some(event) = trigger_rx.recv() => {
                let message = match &event.payload {
                    neuromancer_core::trigger::TriggerPayload::Message { text } => text.clone(),
                    neuromancer_core::trigger::TriggerPayload::CronFire { rendered_instruction, .. } => rendered_instruction.clone(),
                    neuromancer_core::trigger::TriggerPayload::A2aRequest { content, .. } => content.to_string(),
                };
                let trigger_type = event.trigger_type;
                let trigger_id = event.trigger_id.clone();
                let rt = runtime_for_triggers.clone();
                tokio::spawn(async move {
                    match rt.turn_with_trigger(message, trigger_type).await {
                        Ok(result) => {
                            info!(
                                trigger_id = %trigger_id,
                                turn_id = %result.turn_id,
                                "trigger_turn_completed"
                            );
                        }
                        Err(err) => {
                            error!(
                                trigger_id = %trigger_id,
                                error = ?err,
                                "trigger_turn_failed"
                            );
                        }
                    }
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // 8. Graceful shutdown sequence (§20.3)
    // -----------------------------------------------------------------------
    info!("graceful shutdown: stopping trigger sources");
    trigger_manager.shutdown().await;

    // Drain in-flight tasks with timeout
    info!("graceful shutdown: draining in-flight tasks (30s timeout)");
    let drain_timeout = Duration::from_secs(30);
    match tokio::time::timeout(
        drain_timeout,
        system0_runtime.graceful_shutdown(drain_timeout),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!("runtime shutdown failed: {err}"),
        Err(timeout_err) => error!("runtime shutdown timed out: {timeout_err}"),
    }

    // Shut down MCP servers
    if let Some(ref pool) = mcp_pool {
        info!("graceful shutdown: stopping MCP servers");
        pool.shutdown_all().await;
    }

    // Wait for admin server to finish
    info!("graceful shutdown: stopping admin API");
    let _ = rpc_handle.await;

    // Flush OTEL spans (handled by TelemetryGuard drop, but explicit for clarity)
    info!("graceful shutdown: flushing OTEL spans");
    _telemetry_guard.flush();

    info!("neuromancerd stopped");

    Ok(())
}

/// Initialize the composite secrets broker from config.
///
/// Creates a SQLite pool for encrypted storage, loads the master key from
/// the OS keychain, and syncs TOML-configured secret entries.
async fn init_secrets_broker(
    config: &neuromancer_core::config::NeuromancerConfig,
) -> Result<Arc<dyn neuromancer_core::secrets::SecretsBroker>> {
    let data_dir = std::path::Path::new(&config.global.data_dir);
    tokio::fs::create_dir_all(data_dir).await?;

    let secrets_db_path = data_dir.join("secrets.sqlite");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(
                &format!("sqlite://{}", secrets_db_path.display()),
            )?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await?;

    let keyring_service = config
        .secrets
        .keyring_service
        .as_deref()
        .unwrap_or("neuromancer");
    let master_key = neuromancer_secrets::keychain::get_or_create_master_key(keyring_service)
        .map_err(|err| anyhow!("keychain: {err}"))?;

    let broker = neuromancer_secrets::CompositeSecretsBroker::new(
        pool,
        master_key,
        config.secrets.entries.clone(),
    )
    .await
    .map_err(|err| anyhow!("secrets broker: {err}"))?;

    info!(
        entries = config.secrets.entries.len(),
        "secrets broker initialized"
    );
    Ok(Arc::new(broker))
}

use std::str::FromStr;
