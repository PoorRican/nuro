use std::collections::HashMap;

use gray_matter::engine::YAML;
use gray_matter::Matter;

use crate::SkillError;

/// Parsed skill frontmatter metadata.
#[derive(Debug, Clone, Default)]
pub struct SkillMetadata {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Neuromancer-specific permission requirements.
    pub requires: SkillRequires,
    /// Execution mode settings.
    pub execution: SkillExecution,
    /// Model preferences.
    pub models: SkillModels,
    /// Safeguard rules.
    pub safeguards: SkillSafeguards,
    /// Raw extra fields not captured above.
    pub extra: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRequires {
    pub mcp_servers: Vec<String>,
    pub secrets: Vec<String>,
    pub memory_partitions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SkillExecution {
    pub mode: String,
    pub network: Option<String>,
}

impl Default for SkillExecution {
    fn default() -> Self {
        Self {
            mode: "isolated".into(),
            network: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillModels {
    pub preferred: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillSafeguards {
    pub human_approval: Vec<String>,
}

/// Parse a SKILL.md file content into metadata + body.
pub fn parse_skill_file(content: &str) -> Result<(SkillMetadata, String), SkillError> {
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(content);

    let data = parsed
        .data
        .ok_or_else(|| SkillError::ParseError("no frontmatter found".into()))?;

    let metadata = extract_metadata(&data)?;
    Ok((metadata, parsed.content))
}

/// Extract SkillMetadata from gray_matter Pod data.
fn extract_metadata(pod: &gray_matter::Pod) -> Result<SkillMetadata, SkillError> {
    let hash = pod
        .as_hashmap()
        .map_err(|_| SkillError::ParseError("frontmatter is not a mapping".into()))?;

    let name = get_string(&hash, "name")
        .ok_or_else(|| SkillError::MissingField("name".into()))?;
    let version = get_string(&hash, "version").unwrap_or_else(|| "0.0.0".into());
    let description =
        get_string(&hash, "description").unwrap_or_default();

    // Parse metadata.neuromancer section.
    let neuromancer = hash
        .get("metadata")
        .and_then(|m| m.as_hashmap().ok())
        .and_then(|m| m.get("neuromancer").cloned())
        .and_then(|n| n.as_hashmap().ok());

    let (requires, execution, models, safeguards) = if let Some(nm) = neuromancer {
        (
            parse_requires(&nm),
            parse_execution(&nm),
            parse_models(&nm),
            parse_safeguards(&nm),
        )
    } else {
        (
            SkillRequires::default(),
            SkillExecution::default(),
            SkillModels::default(),
            SkillSafeguards::default(),
        )
    };

    Ok(SkillMetadata {
        name,
        version,
        description,
        requires,
        execution,
        models,
        safeguards,
        extra: HashMap::new(),
    })
}

fn parse_requires(nm: &HashMap<String, gray_matter::Pod>) -> SkillRequires {
    let req = nm
        .get("requires")
        .and_then(|r| r.as_hashmap().ok());

    match req {
        Some(r) => SkillRequires {
            mcp_servers: get_string_vec(&r, "mcp_servers"),
            secrets: get_string_vec(&r, "secrets"),
            memory_partitions: get_string_vec(&r, "memory_partitions"),
        },
        None => SkillRequires::default(),
    }
}

fn parse_execution(nm: &HashMap<String, gray_matter::Pod>) -> SkillExecution {
    let exec = nm
        .get("execution")
        .and_then(|e| e.as_hashmap().ok());

    match exec {
        Some(e) => SkillExecution {
            mode: get_string(&e, "mode").unwrap_or_else(|| "isolated".into()),
            network: get_string(&e, "network"),
        },
        None => SkillExecution::default(),
    }
}

fn parse_models(nm: &HashMap<String, gray_matter::Pod>) -> SkillModels {
    let m = nm
        .get("models")
        .and_then(|m| m.as_hashmap().ok());

    match m {
        Some(m) => SkillModels {
            preferred: get_string(&m, "preferred"),
        },
        None => SkillModels::default(),
    }
}

fn parse_safeguards(nm: &HashMap<String, gray_matter::Pod>) -> SkillSafeguards {
    let s = nm
        .get("safeguards")
        .and_then(|s| s.as_hashmap().ok());

    match s {
        Some(s) => SkillSafeguards {
            human_approval: get_string_vec(&s, "human_approval"),
        },
        None => SkillSafeguards::default(),
    }
}

/// Extract a string value from a Pod hashmap.
fn get_string(map: &HashMap<String, gray_matter::Pod>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_string().ok())
}

/// Extract a Vec<String> from a Pod hashmap (expects an array of strings).
fn get_string_vec(map: &HashMap<String, gray_matter::Pod>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(|v| v.as_vec().ok())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL_MD: &str = r#"---
name: "browser_summarize"
version: "0.1.0"
description: "Browse a page and summarize it"
metadata:
  neuromancer:
    requires:
      mcp_servers: ["playwright"]
      secrets: []
      memory_partitions: ["workspace:default"]
    execution:
      mode: "isolated"
      network: "egress-web"
    models:
      preferred: "browser_model"
    safeguards:
      human_approval:
        - "purchase"
        - "file_write_outside_workspace"
---
# Browser Summarize

Instructions for the agent to browse and summarize a page.
"#;

    #[test]
    fn parse_full_skill() {
        let (meta, body) = parse_skill_file(SKILL_MD).unwrap();
        assert_eq!(meta.name, "browser_summarize");
        assert_eq!(meta.version, "0.1.0");
        assert_eq!(meta.description, "Browse a page and summarize it");
        assert_eq!(meta.requires.mcp_servers, vec!["playwright"]);
        assert!(meta.requires.secrets.is_empty());
        assert_eq!(
            meta.requires.memory_partitions,
            vec!["workspace:default"]
        );
        assert_eq!(meta.execution.mode, "isolated");
        assert_eq!(meta.execution.network.as_deref(), Some("egress-web"));
        assert_eq!(meta.models.preferred.as_deref(), Some("browser_model"));
        assert_eq!(
            meta.safeguards.human_approval,
            vec!["purchase", "file_write_outside_workspace"]
        );
        assert!(body.contains("Browser Summarize"));
    }

    #[test]
    fn parse_minimal_skill() {
        let content = r#"---
name: "simple"
---
Do something simple.
"#;
        let (meta, body) = parse_skill_file(content).unwrap();
        assert_eq!(meta.name, "simple");
        assert_eq!(meta.version, "0.0.0");
        assert!(meta.requires.mcp_servers.is_empty());
        assert_eq!(meta.execution.mode, "isolated");
        assert!(body.contains("Do something simple."));
    }

    #[test]
    fn parse_missing_name() {
        let content = r#"---
version: "1.0"
---
body
"#;
        let result = parse_skill_file(content);
        assert!(result.is_err());
    }

    #[test]
    fn parse_no_frontmatter() {
        let content = "Just plain markdown with no frontmatter.";
        let result = parse_skill_file(content);
        assert!(result.is_err());
    }
}
