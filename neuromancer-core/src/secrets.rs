use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::error::NeuromancerError;
use crate::tool::AgentContext;

pub type SecretRef = String;

/// Classification of secret content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// API keys, bearer tokens, passwords.
    #[default]
    Credential,
    /// Browser session cookies/tokens. Supports agent write-back and TTL expiry.
    BrowserSession,
    /// TLS certificates, SSH keys. Injected via file mount.
    CertificateOrKey,
}

/// How a resolved secret should be injected into a tool execution context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretInjectionMode {
    /// Primary mode. `{{SECRET_ID}}` tokens in tool call arguments are replaced
    /// with the plaintext value at execution time via `argument_tokens::expand_tokens`.
    HandleReplacement,

    /// Set as an environment variable for an MCP child process.
    /// Discouraged — prefer HandleReplacement.
    EnvVar { name: String },

    /// Set as an HTTP header.
    Header { name: String },

    /// Mount as a file (e.g. TLS certs, SSH keys).
    FileMount { path: String },

    /// Browser cookie jar — value is JSON-serialized cookies.
    /// Injected as a temp file passed to the browser MCP server.
    CookieJar { domain: String },
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
    pub kind: SecretKind,
    pub allowed_agents: Vec<AgentId>,
    pub allowed_skills: Vec<String>,
    pub allowed_mcp_servers: Vec<String>,
    pub injection_modes: Vec<SecretInjectionMode>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
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

    /// Store a session value from an agent (ACL-checked).
    /// Only allowed for BrowserSession-kind entries where the agent
    /// is in the entry's allowed_agents list.
    async fn store_session(
        &self,
        ctx: &AgentContext,
        id: &str,
        value: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), NeuromancerError>;
}
