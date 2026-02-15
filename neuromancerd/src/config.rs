use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;
use tracing::{error, info, warn};

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::xdg::{XdgLayout, resolve_path, validate_markdown_prompt_file};

/// Load and deserialize config from a TOML file.
pub fn load_config(path: &Path) -> Result<NeuromancerConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading config: {}", path.display()))?;
    let config: NeuromancerConfig =
        toml::from_str(&content).with_context(|| format!("parsing config: {}", path.display()))?;
    Ok(config)
}

/// Validate config for internal consistency:
/// - model slots referenced by agents exist
/// - cron task templates reference agents that exist
pub fn validate_config(config: &NeuromancerConfig, config_path: &Path) -> Result<()> {
    if let Some(ref slot_name) = config.orchestrator.model_slot {
        if !config.models.contains_key(slot_name) {
            anyhow::bail!(
                "orchestrator.model_slot '{}' not found in [models]",
                slot_name
            );
        }
    }

    if config.orchestrator.self_improvement.enabled
        && !config
            .agents
            .contains_key(&config.orchestrator.self_improvement.audit_agent_id)
    {
        anyhow::bail!(
            "self-improvement requires audit agent '{}' in [agents]",
            config.orchestrator.self_improvement.audit_agent_id
        );
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

    validate_prompt_config_health(config, config_path)?;

    info!("config validation passed");
    Ok(())
}

/// Spawn a file watcher that sends updated configs on a watch channel when the file changes.
/// Returns the watcher (must be kept alive) and the watch receiver.
pub fn spawn_config_watcher(
    path: &Path,
) -> Result<(RecommendedWatcher, watch::Receiver<Arc<NeuromancerConfig>>)> {
    let initial = load_config(path)?;
    validate_config(&initial, path)?;
    let (tx, rx) = watch::channel(Arc::new(initial));

    let watched_path = path.to_path_buf();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    info!("config file changed, reloading");
                    match load_config(&watched_path) {
                        Ok(new_config) => match validate_config(&new_config, &watched_path) {
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

fn validate_prompt_config_health(config: &NeuromancerConfig, config_path: &Path) -> Result<()> {
    let layout = XdgLayout::from_env().map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let orchestrator_path = resolve_path(
        config.orchestrator.system_prompt_path.as_deref(),
        layout.default_orchestrator_system_prompt_path(),
        config_dir,
        layout.home_dir(),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    validate_markdown_prompt_file(&orchestrator_path).map_err(|err| {
        anyhow::anyhow!(
            "orchestrator system prompt is invalid at '{}': {}. Run 'neuroctl install --config {}' to remediate.",
            orchestrator_path.display(),
            err,
            config_path.display()
        )
    })?;

    for agent_id in config.agents.keys() {
        let agent_cfg = config
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found during validation", agent_id))?;
        let agent_path = resolve_path(
            agent_cfg.system_prompt_path.as_deref(),
            layout.default_agent_system_prompt_path(agent_id),
            config_dir,
            layout.home_dir(),
        )
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;

        validate_markdown_prompt_file(&agent_path).map_err(|err| {
            anyhow::anyhow!(
                "agent '{}' system prompt is invalid at '{}': {}. Run 'neuroctl install --config {}' to remediate.",
                agent_id,
                agent_path.display(),
                err,
                config_path.display()
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_config(
        dir: &Path,
        include_prompts: bool,
        enable_self_improvement: bool,
        include_audit_agent: bool,
    ) -> PathBuf {
        let config_path = dir.join("neuromancer.toml");
        let mut config = r#"
[global]
instance_id = "test"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-double"

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"
max_iterations = 30

[agents.planner]
models.executor = "executor"
system_prompt_path = "prompts/agents/planner/SYSTEM.md"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []
"#;
        if enable_self_improvement {
            config = r#"
[global]
instance_id = "test"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-double"

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"
max_iterations = 30

[orchestrator.self_improvement]
enabled = true
audit_agent_id = "audit-agent"
require_admin_message_for_mutations = true
verify_before_authorize = true
canary_before_promote = true

[orchestrator.self_improvement.thresholds]
max_success_rate_drop_pct = 3.0
max_tool_failure_increase_pct = 2.0
max_policy_denial_increase_pct = 5.0

[agents.planner]
models.executor = "executor"
system_prompt_path = "prompts/agents/planner/SYSTEM.md"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []
"#;
        }
        let config = if include_audit_agent {
            format!(
                "{config}\n[agents.audit-agent]\nmodels.executor = \"executor\"\nsystem_prompt_path = \"prompts/agents/audit-agent/SYSTEM.md\"\ncapabilities.skills = []\ncapabilities.mcp_servers = []\ncapabilities.a2a_peers = []\ncapabilities.secrets = []\ncapabilities.memory_partitions = []\ncapabilities.filesystem_roots = []\n"
            )
        } else {
            config.to_string()
        };
        fs::write(&config_path, config).expect("config write");

        if include_prompts {
            let orchestrator_path = dir.join("prompts/orchestrator/SYSTEM.md");
            let planner_path = dir.join("prompts/agents/planner/SYSTEM.md");
            fs::create_dir_all(
                orchestrator_path
                    .parent()
                    .expect("orchestrator parent should exist"),
            )
            .expect("orchestrator dir");
            fs::create_dir_all(planner_path.parent().expect("planner parent should exist"))
                .expect("planner dir");
            fs::write(&orchestrator_path, "# System0").expect("orchestrator prompt");
            fs::write(&planner_path, "# Planner").expect("planner prompt");
            if include_audit_agent {
                let audit_path = dir.join("prompts/agents/audit-agent/SYSTEM.md");
                fs::create_dir_all(audit_path.parent().expect("audit parent should exist"))
                    .expect("audit dir");
                fs::write(&audit_path, "# Audit").expect("audit prompt");
            }
        }

        config_path
    }

    #[test]
    fn validate_config_fails_when_prompt_files_are_missing() {
        let dir = temp_dir("nm_cfg_missing_prompts");
        let config_path = write_config(&dir, false, false, false);
        let config = load_config(&config_path).expect("load");
        let err = validate_config(&config, &config_path).expect_err("missing prompts must fail");
        assert!(err.to_string().contains("system prompt is invalid"));
    }

    #[test]
    fn validate_config_succeeds_when_prompt_files_exist() {
        let dir = temp_dir("nm_cfg_with_prompts");
        let config_path = write_config(&dir, true, false, false);
        let config = load_config(&config_path).expect("load");
        validate_config(&config, &config_path).expect("validation should pass");
    }

    #[test]
    fn validate_config_fails_when_self_improvement_audit_agent_missing() {
        let dir = temp_dir("nm_cfg_missing_audit_agent");
        let config_path = write_config(&dir, true, true, false);
        let config = load_config(&config_path).expect("load");
        let err = validate_config(&config, &config_path).expect_err("validation should fail");
        assert!(
            err.to_string()
                .contains("self-improvement requires audit agent")
        );
    }

    #[test]
    fn validate_config_succeeds_when_self_improvement_audit_agent_present() {
        let dir = temp_dir("nm_cfg_with_audit_agent");
        let config_path = write_config(&dir, true, true, true);
        let config = load_config(&config_path).expect("load");
        validate_config(&config, &config_path).expect("validation should pass");
    }
}
