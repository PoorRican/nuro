use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::{AgentContext, ToolCall, ToolResult};

/// Result of a policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String, code: String },
    RequireApproval { question: String },
}

impl PolicyDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Chat message for policy inspection (pre-LLM-call gate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Stacked policy gates evaluated before/after tool and LLM calls.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn pre_tool_call(
        &self,
        ctx: &AgentContext,
        call: &ToolCall,
    ) -> PolicyDecision;

    async fn pre_llm_call(
        &self,
        ctx: &AgentContext,
        messages: &[ChatMessage],
    ) -> PolicyDecision;

    async fn post_tool_call(
        &self,
        ctx: &AgentContext,
        result: &ToolResult,
    ) -> PolicyDecision;
}
