use neuromancer_core::agent::SubAgentReport;
use neuromancer_core::error::{LlmError, NeuromancerError};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};

use crate::conversation::{ChatMessage, ConversationContext};

use super::AgentRuntime;

impl AgentRuntime {
    pub(super) async fn try_recover_invalid_tool_call(
        &self,
        err: &NeuromancerError,
        conversation: &mut ConversationContext,
        task_id: uuid::Uuid,
        available_tool_names: &[String],
        recovery_attempts: &mut u32,
    ) -> Result<bool, NeuromancerError> {
        let NeuromancerError::Llm(LlmError::InvalidResponse { reason }) = err else {
            return Ok(false);
        };
        if !is_invalid_tool_call_reason(reason) {
            return Ok(false);
        }

        let attempted_tool_id =
            extract_attempted_tool_name(reason).unwrap_or_else(|| "__invalid_tool__".to_string());
        let available_tools_display = if available_tool_names.is_empty() {
            "none".to_string()
        } else {
            available_tool_names.join(", ")
        };

        tracing::warn!(
            task_id = %task_id,
            agent_id = %self.config.id,
            attempted_tool_id = %attempted_tool_id,
            available_tools = %available_tools_display,
            retry_attempt = *recovery_attempts + 1,
            retry_limit = self.tool_call_retry_limit,
            "llm_bad_tool_call_detected"
        );

        if *recovery_attempts >= self.tool_call_retry_limit {
            tracing::error!(
                task_id = %task_id,
                agent_id = %self.config.id,
                attempted_tool_id = %attempted_tool_id,
                retries = *recovery_attempts,
                retry_limit = self.tool_call_retry_limit,
                "llm_bad_tool_call_retry_exhausted"
            );
            return Err(NeuromancerError::Llm(LlmError::InvalidResponse {
                reason: format!(
                    "invalid tool-call recovery exhausted after {} retries: {reason}",
                    self.tool_call_retry_limit
                ),
            }));
        }

        *recovery_attempts += 1;
        let call_id = format!("invalid-tool-recovery-{}", *recovery_attempts);
        let recovery_error = format!(
            "Tool '{attempted_tool_id}' is unavailable or invalid. Available tools: {available_tools_display}. Original error: {reason}"
        );

        conversation.add_message(ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: call_id.clone(),
            tool_id: attempted_tool_id.clone(),
            arguments: serde_json::json!({}),
        }]));
        conversation.add_message(ChatMessage::tool_result(ToolResult {
            call_id,
            output: ToolOutput::Error(recovery_error.clone()),
        }));

        self.send_report(SubAgentReport::ToolFailure {
            task_id,
            tool_id: attempted_tool_id.clone(),
            error: recovery_error,
            retry_eligible: *recovery_attempts < self.tool_call_retry_limit,
            attempted_count: *recovery_attempts,
        })
        .await;

        tracing::info!(
            task_id = %task_id,
            agent_id = %self.config.id,
            attempted_tool_id = %attempted_tool_id,
            recovery_attempt = *recovery_attempts,
            retry_limit = self.tool_call_retry_limit,
            "llm_bad_tool_call_recovered"
        );

        Ok(true)
    }
}

// TODO: [low-pri] This is overfit, and provider-specific
fn is_invalid_tool_call_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("tool call validation failed")
        || lower.contains("attempted to call tool")
        || lower.contains("request.tools")
        || lower.contains("tool_use_failed")
}

// TODO: [low-pri] This is overfit, and provider-specific
fn extract_attempted_tool_name(reason: &str) -> Option<String> {
    for marker in [
        "attempted to call tool '",
        "attempted to call tool \"",
        "\"name\":\"",
        "\"name\": \"",
        "'name':'",
        "'name': '",
    ] {
        if let Some(tool_name) = extract_tool_name_after_marker(reason, marker) {
            return Some(tool_name);
        }
    }
    None
}

fn extract_tool_name_after_marker(reason: &str, marker: &str) -> Option<String> {
    let start = reason.find(marker)?;
    let tail = &reason[start + marker.len()..];
    let mut tool_name = String::new();
    for ch in tail.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/') {
            tool_name.push(ch);
        } else {
            break;
        }
    }
    if tool_name.is_empty() {
        None
    } else {
        Some(tool_name)
    }
}
