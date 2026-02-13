use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::{
    AgentCapabilities, AgentConfig, AgentHealthConfig, AgentMode, AgentModelConfig,
};

/// Top-level Neuromancer configuration loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeuromancerConfig {
    pub global: GlobalConfig,
    #[serde(default)]
    pub otel: OtelConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub models: HashMap<String, ModelSlotConfig>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub a2a: A2aConfig,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub agents: HashMap<String, AgentTomlConfig>,
    #[serde(default)]
    pub triggers: TriggersConfig,
    #[serde(default)]
    pub admin_api: AdminApiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub instance_id: String,
    pub workspace_dir: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OtelConfig {
    pub service_name: Option<String>,
    pub otlp_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsConfig {
    pub backend: String,
    pub keyring_service: Option<String>,
    pub require_acl: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            backend: "local_encrypted".into(),
            keyring_service: None,
            require_acl: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub backend: String,
    pub sqlite_path: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            sqlite_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSlotConfig {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub kind: McpServerKind,
    pub command: Option<Vec<String>>,
    pub url: Option<String>,
    pub sandbox: Option<String>,
    pub allowed_roots: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerKind {
    ChildProcess,
    Remote,
    Builtin,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct A2aConfig {
    pub bind_addr: Option<String>,
    pub agent_card_signing: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorConfig {
    pub model_slot: Option<String>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub system_prompt_path: Option<String>,
    #[serde(default = "default_orchestrator_max_iterations")]
    pub max_iterations: u32,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model_slot: None,
            capabilities: AgentCapabilities::default(),
            system_prompt_path: None,
            max_iterations: default_orchestrator_max_iterations(),
        }
    }
}

fn default_orchestrator_max_iterations() -> u32 {
    30
}

/// Per-agent config as it appears in TOML (slightly different shape from runtime AgentConfig).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTomlConfig {
    #[serde(default = "default_agent_mode")]
    pub mode: AgentMode,
    pub image: Option<String>,
    #[serde(default)]
    pub models: AgentModelConfig,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub health: AgentHealthConfig,
    pub system_prompt_path: Option<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
}

fn default_agent_mode() -> AgentMode {
    AgentMode::Inproc
}

fn default_max_iterations() -> u32 {
    20
}

impl AgentTomlConfig {
    pub fn to_agent_config(&self, id: &str, system_prompt: String) -> AgentConfig {
        AgentConfig {
            id: id.to_string(),
            mode: self.mode.clone(),
            image: self.image.clone(),
            models: self.models.clone(),
            capabilities: self.capabilities.clone(),
            health: self.health.clone(),
            system_prompt,
            max_iterations: self.max_iterations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TriggersConfig {
    pub discord: Option<DiscordTriggerConfig>,
    #[serde(default)]
    pub cron: Vec<CronTriggerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordTriggerConfig {
    pub enabled: bool,
    pub token_secret: String,
    #[serde(default)]
    pub allowed_guilds: Vec<String>,
    #[serde(default = "default_dm_policy")]
    pub dm_policy: String,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub channel_routes: Vec<ChannelRouteConfig>,
    #[serde(default)]
    pub response: DiscordResponseConfig,
}

fn default_dm_policy() -> String {
    "disabled".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub per_user_per_minute: u32,
    pub per_channel_per_minute: u32,
    pub max_concurrent_tasks: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            per_user_per_minute: 10,
            per_channel_per_minute: 30,
            max_concurrent_tasks: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRouteConfig {
    pub channel_id: String,
    pub agent: String,
    #[serde(default = "default_thread_mode")]
    pub thread_mode: String,
    #[serde(default = "default_activation")]
    pub activation: String,
}

fn default_thread_mode() -> String {
    "per_task".into()
}

fn default_activation() -> String {
    "mention".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordResponseConfig {
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,
    #[serde(default = "default_overflow")]
    pub overflow: String,
    #[serde(default)]
    pub use_embeds: bool,
    #[serde(default)]
    pub auto_code_blocks: bool,
}

impl Default for DiscordResponseConfig {
    fn default() -> Self {
        Self {
            max_message_length: 2000,
            overflow: "split".into(),
            use_embeds: true,
            auto_code_blocks: true,
        }
    }
}

fn default_max_message_length() -> usize {
    2000
}

fn default_overflow() -> String {
    "split".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTriggerConfig {
    pub id: String,
    pub description: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub schedule: String,
    pub task_template: CronTaskTemplate,
    #[serde(default)]
    pub execution: CronExecutionConfig,
    #[serde(default)]
    pub notification: Option<CronNotificationConfig>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTaskTemplate {
    pub agent: String,
    pub instruction: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronExecutionConfig {
    pub timeout: Option<String>,
    pub idempotency_key: Option<String>,
    pub dedup_window: Option<String>,
    #[serde(default = "default_on_failure")]
    pub on_failure: String,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl Default for CronExecutionConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            idempotency_key: None,
            dedup_window: None,
            on_failure: "retry".into(),
            max_retries: 2,
        }
    }
}

fn default_on_failure() -> String {
    "retry".into()
}

fn default_max_retries() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronNotificationConfig {
    pub on_success: Option<NotificationTarget>,
    pub on_failure: Option<NotificationTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationTarget {
    pub channel: String,
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminApiConfig {
    #[serde(default = "default_admin_bind")]
    pub bind_addr: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for AdminApiConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:9090".into(),
            enabled: true,
        }
    }
}

fn default_admin_bind() -> String {
    "127.0.0.1:9090".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config_toml(extra_orchestrator: &str, extra_agent: &str) -> String {
        format!(
            r#"
[global]
instance_id = "t"
workspace_dir = "/tmp"
data_dir = "/tmp"

[orchestrator]
{extra_orchestrator}

[agents.planner]
models.executor = "executor"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []
{extra_agent}
"#
        )
    }

    #[test]
    fn system_prompt_path_deserializes_for_orchestrator_and_agent() {
        let toml = minimal_config_toml(
            r#"system_prompt_path = "prompts/orchestrator/SYSTEM.md""#,
            r#"system_prompt_path = "prompts/agents/planner/SYSTEM.md""#,
        );
        let cfg: NeuromancerConfig = toml::from_str(&toml).expect("config should parse");
        assert_eq!(
            cfg.orchestrator.system_prompt_path.as_deref(),
            Some("prompts/orchestrator/SYSTEM.md")
        );
        assert_eq!(
            cfg.agents
                .get("planner")
                .and_then(|agent| agent.system_prompt_path.as_deref()),
            Some("prompts/agents/planner/SYSTEM.md")
        );
    }

}
