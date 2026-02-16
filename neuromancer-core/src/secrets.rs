use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::error::NeuromancerError;
use crate::tool::AgentContext;

pub type SecretRef = String;

/// How a resolved secret should be injected into a tool execution context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretInjectionMode {
    /// Set as an environment variable.
    EnvVar { name: String },
    /// Set as an HTTP header.
    Header { name: String },
    /// Mount as a file.
    FileMount { path: String },
}

/// What the secret is being used for (audit trail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretUsage {
    pub tool_id: String,
    pub purpose: String,
}

/// A resolved secret value — opaque to agents, only used by tool execution.
/// The inner value is zeroized on drop.
#[derive(Debug)]
pub struct ResolvedSecret {
    pub value: String,
    pub injection_mode: SecretInjectionMode,
}

/// ACL entry for a secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretAcl {
    pub secret_id: String,
    pub allowed_agents: Vec<AgentId>,
    pub allowed_skills: Vec<String>,
    pub allowed_mcp_servers: Vec<String>,
    pub injection_modes: Vec<SecretInjectionMode>,
}

/// Handle-based secret resolution. Secrets never enter LLM context.
#[async_trait]
pub trait SecretsBroker: Send + Sync {
    /// Resolve a secret handle for tool execution. Validates ACLs.
    async fn resolve_handle_for_tool(
        &self,
        ctx: &AgentContext,
        secret_ref: SecretRef,
        usage: SecretUsage,
    ) -> Result<ResolvedSecret, NeuromancerError>;

    /// List secret handles visible to this agent (not values).
    async fn list_handles(&self, ctx: &AgentContext) -> Result<Vec<String>, NeuromancerError>;

    /// Store a new secret (admin operation).
    async fn store(&self, id: &str, value: &str, acl: SecretAcl) -> Result<(), NeuromancerError>;

    /// Revoke/delete a secret.
    async fn revoke(&self, id: &str) -> Result<(), NeuromancerError>;
}
