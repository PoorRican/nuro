use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::error::NeuromancerError;

/// Specification of a tool available to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Builtin,
    Mcp { server_id: String },
    A2a { peer_id: AgentId },
    Skill { skill_id: String },
}

/// A tool call request from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub tool_id: String,
    pub arguments: serde_json::Value,
}

/// Result of executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Success(serde_json::Value),
    Error(String),
}

/// Context for an agent's tool execution — carries identity and policy info.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub agent_id: AgentId,
    pub task_id: crate::task::TaskId,
    pub allowed_tools: Vec<String>,
    pub allowed_mcp_servers: Vec<String>,
    pub allowed_secrets: Vec<String>,
    pub allowed_memory_partitions: Vec<String>,
}

/// Policy-enforcing tool broker. Wraps tool definitions with capability checks.
#[async_trait]
pub trait ToolBroker: Send + Sync {
    /// List tools visible to this agent context, filtered by policy.
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec>;

    /// Execute a tool call after policy checks.
    async fn call_tool(
        &self,
        ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError>;
}
