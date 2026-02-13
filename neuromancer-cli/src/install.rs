use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::xdg::{XdgLayout, resolve_path};

use crate::CliError;

#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub created: Vec<String>,
    pub existing: Vec<String>,
}

pub fn resolve_install_config_path(config_path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    if let Some(path) = config_path {
        return Ok(path);
    }

    let layout = XdgLayout::from_env().map_err(|err| CliError::Lifecycle(err.to_string()))?;
    Ok(layout.default_config_path())
}

pub fn run_install(config_path: &Path) -> Result<InstallResult, CliError> {
    let raw = fs::read_to_string(config_path).map_err(|err| {
        CliError::Usage(format!(
            "failed to read config '{}': {err}",
            config_path.display()
        ))
    })?;
    let config: NeuromancerConfig = toml::from_str(&raw).map_err(|err| {
        CliError::Usage(format!(
            "failed to parse config '{}': {err}",
            config_path.display()
        ))
    })?;

    let layout = XdgLayout::from_env().map_err(|err| CliError::Lifecycle(err.to_string()))?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let mut created = Vec::new();
    let mut existing = Vec::new();

    ensure_dir(&layout.config_root(), &mut created, &mut existing)?;
    ensure_dir(&layout.runtime_root(), &mut created, &mut existing)?;
    ensure_dir(&layout.skills_dir(), &mut created, &mut existing)?;

    let orchestrator_prompt_path = resolve_path(
        config.orchestrator.system_prompt_path.as_deref(),
        layout.default_orchestrator_system_prompt_path(),
        config_dir,
        layout.home_dir(),
    )
    .map_err(|err| CliError::Usage(err.to_string()))?;
    ensure_prompt_file(
        &orchestrator_prompt_path,
        default_orchestrator_system_prompt(),
        &mut created,
        &mut existing,
    )?;

    for agent_id in config.agents.keys() {
        let agent_cfg = config
            .agents
            .get(agent_id)
            .ok_or_else(|| CliError::Lifecycle(format!("agent '{}' not found", agent_id)))?;
        let prompt_path = resolve_path(
            agent_cfg.system_prompt_path.as_deref(),
            layout.default_agent_system_prompt_path(agent_id),
            config_dir,
            layout.home_dir(),
        )
        .map_err(|err| CliError::Usage(err.to_string()))?;

        ensure_prompt_file(
            &prompt_path,
            &default_agent_system_prompt(agent_id),
            &mut created,
            &mut existing,
        )?;
    }

    Ok(InstallResult { created, existing })
}

fn ensure_dir(path: &Path, created: &mut Vec<String>, existing: &mut Vec<String>) -> Result<(), CliError> {
    if path.exists() {
        existing.push(path.display().to_string());
        return Ok(());
    }

    fs::create_dir_all(path).map_err(|err| {
        CliError::Lifecycle(format!("failed to create directory '{}': {err}", path.display()))
    })?;
    created.push(path.display().to_string());
    Ok(())
}

fn ensure_prompt_file(
    path: &Path,
    content: &str,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
) -> Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Err(CliError::Lifecycle(format!(
            "invalid prompt path '{}'",
            path.display()
        )));
    };

    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to create prompt directory '{}': {err}",
                parent.display()
            ))
        })?;
        created.push(parent.display().to_string());
    }

    if path.exists() {
        existing.push(path.display().to_string());
        return Ok(());
    }

    fs::write(path, content).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to write prompt file '{}': {err}",
            path.display()
        ))
    })?;
    created.push(path.display().to_string());
    Ok(())
}

fn default_orchestrator_system_prompt() -> &'static str {
    r#"# Neuromancer System0 Prompt

You are {{ORCHESTRATOR_ID}}, the System0 orchestrator for Neuromancer.

## Mission
- Mediate every inbound user/admin turn.
- Plan safely and delegate to the most appropriate sub-agent when needed.
- Keep continuity across turns and use prior conversation context.
- Use control-plane tools for delegation, coordination, and safe runtime changes.

## Available Agents
{{AVAILABLE_AGENTS}}

## Available Control Tools
{{AVAILABLE_TOOLS}}

## Operating Rules
- Respect capability and policy boundaries.
- Prefer explicit delegation to specialized agents for domain work.
- Summarize delegated outcomes back to the user clearly.
- If required information is missing, ask concise clarifying questions.
"#
}

fn default_agent_system_prompt(agent_id: &str) -> String {
    format!(
        r#"# Neuromancer Agent Prompt: {agent_id}

You are agent `{agent_id}`.

## Mission
- Execute delegated instructions accurately.
- Use only allowlisted tools and capabilities.
- Return concise, factual outputs suitable for orchestration.

## Operating Rules
- Do not assume permissions you do not have.
- Report failures clearly with actionable context.
- Prefer deterministic, auditable actions.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}_{nanos}"))
    }

    #[test]
    fn default_orchestrator_prompt_contains_placeholders() {
        let prompt = default_orchestrator_system_prompt();
        assert!(prompt.contains("{{ORCHESTRATOR_ID}}"));
        assert!(prompt.contains("{{AVAILABLE_AGENTS}}"));
        assert!(prompt.contains("{{AVAILABLE_TOOLS}}"));
    }

    #[test]
    fn default_agent_prompt_mentions_agent_name() {
        let prompt = default_agent_system_prompt("planner");
        assert!(prompt.contains("planner"));
    }

    #[test]
    fn ensure_prompt_file_never_overwrites_existing_file() {
        let dir = unique_temp_path("nm_install_prompt");
        fs::create_dir_all(&dir).expect("dir");
        let prompt_path = dir.join("SYSTEM.md");
        fs::write(&prompt_path, "original").expect("write");

        let mut created = Vec::new();
        let mut existing = Vec::new();
        ensure_prompt_file(&prompt_path, "replacement", &mut created, &mut existing)
            .expect("ensure prompt");

        let current = fs::read_to_string(&prompt_path).expect("read");
        assert_eq!(current, "original");
        assert!(existing.iter().any(|p| p == &prompt_path.display().to_string()));
    }
}
