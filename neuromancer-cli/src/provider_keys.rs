use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::xdg::XdgLayout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCredentialTarget {
    pub provider: String,
    pub env_var: String,
    pub path: PathBuf,
}

pub fn provider_credential_targets(
    config: &NeuromancerConfig,
    layout: &XdgLayout,
) -> Vec<ProviderCredentialTarget> {
    let mut providers = BTreeSet::new();
    for slot in config.models.values() {
        let provider = normalize_provider(&slot.provider);
        if provider.is_empty() {
            continue;
        }
        if provider_env_var(&provider).is_none() {
            continue;
        }
        providers.insert(provider);
    }

    providers
        .into_iter()
        .filter_map(|provider| {
            let env_var = provider_env_var(&provider)?;
            let path = provider_key_path(layout, &provider);
            Some(ProviderCredentialTarget {
                provider,
                env_var,
                path,
            })
        })
        .collect()
}

pub fn provider_env_var(provider: &str) -> Option<String> {
    let provider = normalize_provider(provider);
    if provider.is_empty() {
        return None;
    }
    if provider == "mock" {
        return None;
    }

    let mapped = match provider.as_str() {
        "openai" => "OPENAI_API_KEY".to_string(),
        "anthropic" => "ANTHROPIC_API_KEY".to_string(),
        "groq" => "GROQ_API_KEY".to_string(),
        "fireworks" => "FIREWORKS_API_KEY".to_string(),
        "gemini" | "google" => "GEMINI_API_KEY".to_string(),
        "xai" => "XAI_API_KEY".to_string(),
        "mistral" => "MISTRAL_API_KEY".to_string(),
        _ => format!("{}_API_KEY", normalize_for_env_var(&provider)),
    };
    Some(mapped)
}

pub fn read_nonempty_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn read_nonempty_file(path: &Path) -> Result<Option<String>, std::io::Error> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn write_key_file(path: &Path, value: &str) -> Result<(), std::io::Error> {
    fs::write(path, format!("{}\n", value.trim()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn provider_key_path(layout: &XdgLayout, provider: &str) -> PathBuf {
    layout
        .provider_keys_dir()
        .join(format!("{}.key", normalize_for_filename(provider)))
}

fn normalize_provider(provider: &str) -> String {
    provider.trim().to_lowercase()
}

fn normalize_for_env_var(provider: &str) -> String {
    let mut out = String::new();
    for ch in provider.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

fn normalize_for_filename(provider: &str) -> String {
    let normalized = normalize_provider(provider);
    let mut out = String::new();
    for ch in normalized.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::config::NeuromancerConfig;

    #[test]
    fn provider_env_var_is_not_groq_only() {
        assert_eq!(provider_env_var("groq").as_deref(), Some("GROQ_API_KEY"));
        assert_eq!(
            provider_env_var("openai").as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            provider_env_var("anthropic").as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn provider_env_var_maps_fireworks() {
        assert_eq!(
            provider_env_var("fireworks").as_deref(),
            Some("FIREWORKS_API_KEY")
        );
    }

    #[test]
    fn provider_env_var_ignores_mock() {
        assert!(provider_env_var("mock").is_none());
    }

    #[test]
    fn targets_are_unique_and_include_paths() {
        let cfg: NeuromancerConfig = toml::from_str(
            r#"
[global]
instance_id = "t"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "groq"
model = "a"

[models.planner]
provider = "openai"
model = "b"

[orchestrator]
model_slot = "executor"

[admin_api]
bind_addr = "127.0.0.1:9090"
enabled = true
"#,
        )
        .expect("config parse");
        let layout = XdgLayout::new(PathBuf::from("/tmp/home"));
        let targets = provider_credential_targets(&cfg, &layout);
        assert_eq!(targets.len(), 2);
        assert!(
            targets
                .iter()
                .any(|target| target.env_var == "GROQ_API_KEY")
        );
        assert!(
            targets
                .iter()
                .any(|target| target.env_var == "OPENAI_API_KEY")
        );
        assert!(
            targets
                .iter()
                .all(|target| target.path.starts_with(layout.provider_keys_dir()))
        );
    }
}
