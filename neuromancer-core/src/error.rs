use std::time::Duration;

use crate::agent::AgentId;
use crate::secrets::SecretRef;
use crate::task::TaskId;

#[derive(Debug, thiserror::Error)]
pub enum NeuromancerError {
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),

    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("policy error: {0}")]
    Policy(#[from] PolicyError),

    #[error("infra error: {0}")]
    Infra(#[from] InfraError),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("max iterations exceeded for task {task_id}: {iterations} iterations")]
    MaxIterationsExceeded { task_id: TaskId, iterations: u32 },

    #[error("invalid tool call to {tool_id}: {reason}")]
    InvalidToolCall { tool_id: String, reason: String },

    #[error("context overflow: budget={budget}, used={used}")]
    ContextOverflow { budget: u32, used: u32 },

    #[error("checkpoint corrupted for task {task_id}")]
    CheckpointCorrupted { task_id: TaskId },

    #[error("timeout for task {task_id} after {elapsed:?}")]
    Timeout { task_id: TaskId, elapsed: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider {provider} unavailable (status {status})")]
    ProviderUnavailable { provider: String, status: u16 },

    #[error("rate limited by {provider}, retry after {retry_after:?}")]
    RateLimited {
        provider: String,
        retry_after: Duration,
    },

    #[error("invalid LLM response: {reason}")]
    InvalidResponse { reason: String },

    #[error("content filtered: {reason}")]
    ContentFiltered { reason: String },
}

#[derive(Debug, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum ToolError {
    #[error("tool not found: {tool_id}")]
    NotFound { tool_id: String },

    #[error("tool {tool_id} execution failed: {message}")]
    ExecutionFailed { tool_id: String, message: String },

    #[error("tool {tool_id} timed out after {elapsed:?}")]
    Timeout { tool_id: String, elapsed: Duration },

    #[error("MCP server down: {server_id}")]
    McpServerDown { server_id: String },

    #[error("capability denied for agent {agent_id}: {capability}")]
    CapabilityDenied {
        agent_id: AgentId,
        capability: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("capability denied for agent {agent_id}: {capability}")]
    CapabilityDenied {
        agent_id: AgentId,
        capability: CapabilityRef,
    },

    #[error("secret access denied for agent {agent_id}: {secret_ref}")]
    SecretAccessDenied {
        agent_id: AgentId,
        secret_ref: SecretRef,
    },

    #[error("partition access denied for agent {agent_id}: {partition}")]
    PartitionAccessDenied {
        agent_id: AgentId,
        partition: String,
    },

    #[error("heuristic blocked pattern '{pattern}': {description}")]
    HeuristicBlocked {
        pattern: String,
        description: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum InfraError {
    #[error("database error: {0}")]
    Database(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("container runtime error: {0}")]
    ContainerRuntime(String),
}

pub type CapabilityRef = String;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_denied_formats_and_serializes() {
        let error = ToolError::CapabilityDenied {
            agent_id: "planner".to_string(),
            capability: "can_request:browser".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "capability denied for agent planner: can_request:browser"
        );

        let encoded = serde_json::to_string(&error).expect("serialize");
        let decoded: ToolError = serde_json::from_str(&encoded).expect("deserialize");
        match decoded {
            ToolError::CapabilityDenied {
                agent_id,
                capability,
            } => {
                assert_eq!(agent_id, "planner");
                assert_eq!(capability, "can_request:browser");
            }
            other => panic!("expected capability denied, got {other:?}"),
        }
    }
}
