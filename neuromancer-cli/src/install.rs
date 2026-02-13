use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::xdg::{XdgLayout, resolve_path};

use crate::CliError;
use crate::provider_keys::{
    ProviderCredentialTarget, provider_credential_targets, read_nonempty_file, write_key_file,
};

#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub created: Vec<String>,
    pub existing: Vec<String>,
    pub overwritten: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub fn resolve_config_path(config_path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    if let Some(path) = config_path {
        return Ok(path);
    }

    let layout = XdgLayout::from_env().map_err(|err| CliError::Lifecycle(err.to_string()))?;
    Ok(layout.default_config_path())
}

pub fn run_install(config_path: &Path, override_config: bool) -> Result<InstallResult, CliError> {
    let layout = XdgLayout::from_env().map_err(|err| CliError::Lifecycle(err.to_string()))?;
    let defaults_root = defaults_root();
    let bootstrap_dir = defaults_root.join("bootstrap");
    let default_config_src = bootstrap_dir.join("neuromancer.toml");
    let default_orchestrator_prompt_src = bootstrap_dir.join("orchestrator/SYSTEM.md");
    let default_agent_prompt_src = defaults_root.join("templates/agent/SYSTEM.md");

    let mut created = Vec::new();
    let mut existing = Vec::new();
    let mut overwritten = Vec::new();
    let mut warnings = Vec::new();

    copy_defaults_tree(&bootstrap_dir, &layout.config_root(), &mut created, &mut existing)?;
    ensure_dir(&layout.runtime_root(), &mut created, &mut existing)?;
    ensure_dir(&layout.runtime_home_root(), &mut created, &mut existing)?;
    ensure_dir(&layout.provider_keys_dir(), &mut created, &mut existing)?;
    ensure_dir(&layout.skills_dir(), &mut created, &mut existing)?;

    // When --config points somewhere other than the default XDG config file, bootstrap it too.
    if config_path != layout.default_config_path() {
        ensure_file_from_source(config_path, &default_config_src, &mut created, &mut existing)?;
    }
    if override_config {
        overwrite_file_from_source(
            config_path,
            &default_config_src,
            &mut created,
            &mut overwritten,
        )?;
    }

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

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let orchestrator_prompt_path = resolve_path(
        config.orchestrator.system_prompt_path.as_deref(),
        layout.default_orchestrator_system_prompt_path(),
        config_dir,
        layout.home_dir(),
    )
    .map_err(|err| CliError::Usage(err.to_string()))?;
    let default_orchestrator_prompt = load_template(&default_orchestrator_prompt_src)?;
    ensure_prompt_file(
        &orchestrator_prompt_path,
        &default_orchestrator_prompt,
        &mut created,
        &mut existing,
    )?;

    let default_agent_prompt = load_template(&default_agent_prompt_src)?;
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
            &default_agent_prompt,
            &mut created,
            &mut existing,
        )?;
    }

    bootstrap_provider_keys(
        &config,
        &layout,
        &mut created,
        &mut existing,
        &mut warnings,
    )?;

    Ok(InstallResult {
        created,
        existing,
        overwritten,
        warnings,
    })
}

fn bootstrap_provider_keys(
    config: &NeuromancerConfig,
    layout: &XdgLayout,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(), CliError> {
    let targets = provider_credential_targets(config, layout);
    if targets.is_empty() {
        return Ok(());
    }

    warnings.push(
        "Provider API keys are stored as plaintext files under XDG_RUNTIME_HOME for v0.1-alpha. OS-specific keychain integration MUST be implemented.".to_string(),
    );

    if !io::stdin().is_terminal() {
        warnings.push(
            "Install is running without an interactive terminal; provider key prompts were skipped."
                .to_string(),
        );
        return Ok(());
    }

    eprintln!(
        "warning: provider keys will be written under '{}' (temporary v0.1-alpha behavior)",
        layout.provider_keys_dir().display()
    );
    eprintln!(
        "warning: OS-specific keychain integration MUST be implemented in a follow-up release"
    );

    for target in targets {
        if let Some(existing_key) = read_nonempty_file(&target.path).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to read provider key file '{}': {err}",
                target.path.display()
            ))
        })? {
            if !existing_key.is_empty() {
                existing.push(target.path.display().to_string());
                continue;
            }
        }

        let input = prompt_for_provider_key(&target)?;
        if input.trim().is_empty() {
            warnings.push(format!(
                "No API key provided for provider '{}' (env var {}).",
                target.provider, target.env_var
            ));
            continue;
        }

        write_key_file(&target.path, &input).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to write provider key file '{}': {err}",
                target.path.display()
            ))
        })?;
        created.push(target.path.display().to_string());
    }

    Ok(())
}

fn prompt_for_provider_key(target: &ProviderCredentialTarget) -> Result<String, CliError> {
    eprint!(
        "Enter API key for provider '{}' ({}). Leave blank to skip: ",
        target.provider, target.env_var
    );
    io::stderr()
        .flush()
        .map_err(|err| CliError::Lifecycle(format!("failed to flush prompt: {err}")))?;

    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .map_err(|err| CliError::Lifecycle(format!("failed to read API key input: {err}")))?;
    Ok(buffer.trim().to_string())
}

fn defaults_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.join("defaults")
}

fn load_template(path: &Path) -> Result<String, CliError> {
    fs::read_to_string(path).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to read defaults template '{}': {err}",
            path.display()
        ))
    })
}

fn copy_defaults_tree(
    source_root: &Path,
    destination_root: &Path,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
) -> Result<(), CliError> {
    if !source_root.exists() {
        return Err(CliError::Lifecycle(format!(
            "defaults bootstrap directory not found: '{}'",
            source_root.display()
        )));
    }
    ensure_dir(destination_root, created, existing)?;
    copy_dir_recursive(source_root, destination_root, created, existing)
}

fn copy_dir_recursive(
    source_dir: &Path,
    destination_dir: &Path,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
) -> Result<(), CliError> {
    for entry in fs::read_dir(source_dir).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to read defaults directory '{}': {err}",
            source_dir.display()
        ))
    })? {
        let entry = entry.map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to read defaults directory entry in '{}': {err}",
                source_dir.display()
            ))
        })?;
        let source_path = entry.path();
        let destination_path = destination_dir.join(entry.file_name());
        let metadata = entry.metadata().map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to read metadata for defaults path '{}': {err}",
                source_path.display()
            ))
        })?;

        if metadata.is_dir() {
            ensure_dir(&destination_path, created, existing)?;
            copy_dir_recursive(&source_path, &destination_path, created, existing)?;
            continue;
        }

        ensure_file_from_source(&destination_path, &source_path, created, existing)?;
    }
    Ok(())
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

fn ensure_file_from_source(
    path: &Path,
    source_path: &Path,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
) -> Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Err(CliError::Lifecycle(format!(
            "invalid destination path '{}'",
            path.display()
        )));
    };

    ensure_dir(parent, created, existing)?;

    if path.exists() {
        existing.push(path.display().to_string());
        return Ok(());
    }

    fs::copy(source_path, path).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to copy defaults file '{}' to '{}': {err}",
            source_path.display(),
            path.display(),
        ))
    })?;
    created.push(path.display().to_string());
    Ok(())
}

fn overwrite_file_from_source(
    path: &Path,
    source_path: &Path,
    created: &mut Vec<String>,
    overwritten: &mut Vec<String>,
) -> Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Err(CliError::Lifecycle(format!(
            "invalid destination path '{}'",
            path.display()
        )));
    };

    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to create directory '{}': {err}",
                parent.display()
            ))
        })?;
        created.push(parent.display().to_string());
    }

    let existed = path.exists();
    fs::copy(source_path, path).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to copy defaults file '{}' to '{}': {err}",
            source_path.display(),
            path.display(),
        ))
    })?;

    if existed {
        overwritten.push(path.display().to_string());
    } else {
        created.push(path.display().to_string());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let prompt = load_template(&defaults_root().join("bootstrap/orchestrator/SYSTEM.md"))
            .expect("template should load");
        assert!(prompt.contains("{{ORCHESTRATOR_ID}}"));
        assert!(prompt.contains("{{AVAILABLE_AGENTS}}"));
        assert!(prompt.contains("{{AVAILABLE_TOOLS}}"));
    }

    #[test]
    fn default_agent_prompt_is_generic() {
        let prompt =
            load_template(&defaults_root().join("templates/agent/SYSTEM.md")).expect("template");
        assert!(!prompt.contains("{{AGENT_ID}}"));
        assert!(prompt.contains("delegated sub-agent"));
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

    #[test]
    fn ensure_file_from_source_never_overwrites_existing_file() {
        let dir = unique_temp_path("nm_install_config");
        fs::create_dir_all(&dir).expect("dir");
        let config_path = dir.join("neuromancer.toml");
        fs::write(&config_path, "original").expect("write");
        let source = dir.join("source.toml");
        fs::write(&source, "replacement").expect("write source");

        let mut created = Vec::new();
        let mut existing = Vec::new();
        ensure_file_from_source(&config_path, &source, &mut created, &mut existing)
            .expect("ensure from source");

        let current = fs::read_to_string(&config_path).expect("read");
        assert_eq!(current, "original");
        assert!(existing.iter().any(|p| p == &config_path.display().to_string()));
    }

    #[test]
    fn overwrite_file_from_source_replaces_existing_file() {
        let dir = unique_temp_path("nm_install_override");
        fs::create_dir_all(&dir).expect("dir");
        let config_path = dir.join("neuromancer.toml");
        fs::write(&config_path, "original").expect("write");
        let source = dir.join("source.toml");
        fs::write(&source, "replacement").expect("write source");

        let mut created = Vec::new();
        let mut overwritten = Vec::new();
        overwrite_file_from_source(&config_path, &source, &mut created, &mut overwritten)
            .expect("override from source");

        let current = fs::read_to_string(&config_path).expect("read");
        assert_eq!(current, "replacement");
        assert!(overwritten
            .iter()
            .any(|p| p == &config_path.display().to_string()));
    }

    #[test]
    fn blank_slate_config_has_required_sections() {
        let config =
            load_template(&defaults_root().join("bootstrap/neuromancer.toml")).expect("template");
        assert!(config.contains("[global]"));
        assert!(config.contains("[orchestrator]"));
        assert!(!config.contains("[agents."));
    }
}
