use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;
use tracing::{error, info, warn};

use neuromancer_core::config::NeuromancerConfig;

/// Load and deserialize config from a TOML file.
pub fn load_config(path: &Path) -> Result<NeuromancerConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading config: {}", path.display()))?;
    let config: NeuromancerConfig =
        toml::from_str(&content).with_context(|| format!("parsing config: {}", path.display()))?;
    Ok(config)
}

/// Validate config for internal consistency:
/// - routing rules reference agents that exist
/// - model slots referenced by agents exist
/// - cron task templates reference agents that exist
pub fn validate_config(config: &NeuromancerConfig) -> Result<()> {
    // Check routing default agent exists
    if !config.agents.contains_key(&config.routing.default_agent) {
        anyhow::bail!(
            "routing.default_agent '{}' not found in [agents]",
            config.routing.default_agent
        );
    }

    // Check routing rules reference valid agents
    for rule in &config.routing.rules {
        if !config.agents.contains_key(&rule.agent) {
            anyhow::bail!(
                "routing rule targets agent '{}' which is not defined in [agents]",
                rule.agent
            );
        }
    }

    // Check classifier_model references a defined model slot
    if let Some(ref classifier) = config.routing.classifier_model {
        if !config.models.contains_key(classifier) {
            anyhow::bail!(
                "routing.classifier_model '{}' not found in [models]",
                classifier
            );
        }
    }

    // Check agent model slot references
    for (name, agent) in &config.agents {
        for slot in [
            &agent.models.planner,
            &agent.models.executor,
            &agent.models.verifier,
        ] {
            if let Some(slot_name) = slot {
                if !config.models.contains_key(slot_name) {
                    anyhow::bail!(
                        "agent '{}' references model slot '{}' not found in [models]",
                        name,
                        slot_name
                    );
                }
            }
        }

        // Check MCP server references
        for server in &agent.capabilities.mcp_servers {
            if !config.mcp_servers.contains_key(server) {
                anyhow::bail!(
                    "agent '{}' references mcp_server '{}' not found in [mcp_servers]",
                    name,
                    server
                );
            }
        }

        // Check A2A peer references (must be other agents)
        for peer in &agent.capabilities.a2a_peers {
            if !config.agents.contains_key(peer) {
                anyhow::bail!(
                    "agent '{}' references a2a_peer '{}' not found in [agents]",
                    name,
                    peer
                );
            }
        }
    }

    // Check cron task templates reference valid agents
    for cron in &config.triggers.cron {
        if !config.agents.contains_key(&cron.task_template.agent) {
            anyhow::bail!(
                "cron job '{}' targets agent '{}' not found in [agents]",
                cron.id,
                cron.task_template.agent
            );
        }
    }

    info!("config validation passed");
    Ok(())
}

/// Spawn a file watcher that sends updated configs on a watch channel when the file changes.
/// Returns the watcher (must be kept alive) and the watch receiver.
pub fn spawn_config_watcher(
    path: &Path,
) -> Result<(RecommendedWatcher, watch::Receiver<Arc<NeuromancerConfig>>)> {
    let initial = load_config(path)?;
    validate_config(&initial)?;
    let (tx, rx) = watch::channel(Arc::new(initial));

    let watched_path = path.to_path_buf();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    info!("config file changed, reloading");
                    match load_config(&watched_path) {
                        Ok(new_config) => match validate_config(&new_config) {
                            Ok(()) => {
                                if tx.send(Arc::new(new_config)).is_err() {
                                    warn!("config watch channel closed");
                                }
                                info!("config reloaded successfully");
                            }
                            Err(e) => {
                                error!("new config failed validation, keeping old config: {e}");
                            }
                        },
                        Err(e) => {
                            error!("failed to parse new config, keeping old config: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                error!("config file watcher error: {e}");
            }
        })?;

    watcher.watch(path, RecursiveMode::NonRecursive)?;

    Ok((watcher, rx))
}
