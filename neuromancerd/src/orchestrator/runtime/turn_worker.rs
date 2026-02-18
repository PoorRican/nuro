use std::sync::Arc;
use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_core::memory::{MemoryKind, MemoryQuery, MemoryStore};
use neuromancer_core::rpc::{OrchestratorTurnResult, ThreadEvent};
use neuromancer_core::thread::{ChatMessage, ThreadId, ThreadStore};
use neuromancer_core::tool::AgentContext;
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::error::System0Error;
use crate::orchestrator::state::{SYSTEM0_AGENT_ID, System0ToolBroker};
use crate::orchestrator::threads::compaction;
use crate::orchestrator::tracing::conversation_projection::normalize_error_message;
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, make_event};

use super::extract_response_text;

pub(super) struct System0TurnWorker {
    pub(super) agent_runtime: Arc<AgentRuntime>,
    pub(super) thread_store: Arc<dyn ThreadStore>,
    pub(super) memory_store: Arc<dyn MemoryStore>,
    pub(super) system0_thread_id: ThreadId,
    pub(super) system0_broker: System0ToolBroker,
    pub(super) thread_journal: ThreadJournal,
}

impl System0TurnWorker {
    #[allow(deprecated)]
    pub(super) async fn process_turn(
        &mut self,
        message: String,
        trigger_type: TriggerType,
    ) -> Result<OrchestratorTurnResult, System0Error> {
        let turn_id = uuid::Uuid::new_v4();
        let turn_started_at = Instant::now();
        tracing::info!(
            turn_id = %turn_id,
            trigger_type = ?trigger_type,
            message_chars = message.len(),
            "turn_started"
        );
        self.system0_broker
            .set_turn_context(turn_id, trigger_type, message.clone())
            .await;

        let mut event = system0_event(
            "message_user",
            turn_id,
            serde_json::json!({
                "role": "user", "content": message.clone()
            }),
        );
        event.turn_id = Some(turn_id.to_string());
        let _ = self.thread_journal.append_event(event).await;

        // Load memory summaries for injection into context
        let injected_context = self.load_memory_context(turn_id).await;

        let output = match self
            .agent_runtime
            .execute_turn_with_thread_store(
                &*self.thread_store,
                &self.system0_thread_id,
                TriggerSource::Cli,
                message.clone(),
                turn_id,
                injected_context,
            )
            .await
        {
            Ok(output) => output,
            Err(err) => {
                let normalized = normalize_error_message(err.to_string());
                let mut event = system0_event(
                    "error",
                    turn_id,
                    serde_json::json!({
                        "error": normalized,
                        "message": "orchestrator turn failed",
                    }),
                );
                event.turn_id = Some(turn_id.to_string());
                let _ = self.thread_journal.append_event(event).await;
                tracing::error!(
                    turn_id = %turn_id,
                    error = ?err,
                    duration_ms = turn_started_at.elapsed().as_millis(),
                    "turn_failed"
                );
                return Err(System0Error::Internal(err.to_string()));
            }
        };

        let response =
            extract_response_text(&output.output).unwrap_or_else(|| output.output.summary.clone());
        let delegated_tasks = self.system0_broker.take_delegations(turn_id).await;
        let tool_invocations = self.system0_broker.take_tool_invocations(turn_id).await;

        self.journal_turn_events(turn_id, &tool_invocations, &response, &turn_started_at)
            .await;

        // Post-turn compaction check
        if let Ok(Some(thread)) = self.thread_store.get_thread(&self.system0_thread_id).await {
            let compact_ctx = AgentContext {
                agent_id: SYSTEM0_AGENT_ID.to_string(),
                task_id: turn_id,
                allowed_tools: vec![],
                allowed_mcp_servers: vec![],
                allowed_peer_agents: vec![],
                allowed_secrets: vec![],
                allowed_memory_partitions: vec![
                    "system0".to_string(),
                    "workspace:default".to_string(),
                ],
            };
            if let Err(err) = compaction::maybe_compact_thread(
                &*self.thread_store,
                &*self.memory_store,
                &compact_ctx,
                &thread,
            )
            .await
            {
                tracing::warn!(
                    turn_id = %turn_id,
                    error = ?err,
                    "compaction_failed"
                );
            }
        }

        tracing::info!(
            turn_id = %turn_id,
            delegated_tasks = delegated_tasks.len(),
            tool_invocations = tool_invocations.len(),
            duration_ms = turn_started_at.elapsed().as_millis(),
            "turn_finished"
        );

        Ok(OrchestratorTurnResult {
            turn_id: turn_id.to_string(),
            response,
            delegated_tasks,
            tool_invocations,
        })
    }

    /// Load memory summaries from the System0 partition and format them as
    /// system-role `ChatMessage`s for ephemeral injection into the conversation context.
    async fn load_memory_context(&self, turn_id: uuid::Uuid) -> Vec<ChatMessage> {
        let ctx = AgentContext {
            agent_id: SYSTEM0_AGENT_ID.to_string(),
            task_id: turn_id,
            allowed_tools: vec![],
            allowed_mcp_servers: vec![],
            allowed_peer_agents: vec![],
            allowed_secrets: vec![],
            allowed_memory_partitions: vec![
                "system0".to_string(),
                "workspace:default".to_string(),
            ],
        };

        let query = MemoryQuery {
            partition: Some("system0".to_string()),
            kind: Some(MemoryKind::Summary),
            limit: 20,
            ..Default::default()
        };
        let page = match self.memory_store.query(&ctx, query).await {
            Ok(page) => page,
            Err(err) => {
                tracing::warn!(error = ?err, "memory_context_load_failed");
                return vec![];
            }
        };

        if page.items.is_empty() {
            return vec![];
        }

        let mut parts = Vec::with_capacity(page.items.len() + 1);
        parts.push("[Prior conversation summaries]".to_string());
        for item in &page.items {
            parts.push(item.content.clone());
        }
        let combined = parts.join("\n\n");

        tracing::debug!(
            summaries = page.items.len(),
            chars = combined.len(),
            "memory_context_injected"
        );

        vec![ChatMessage::system(&combined)]
    }

    async fn journal_turn_events(
        &self,
        turn_id: uuid::Uuid,
        invocations: &[neuromancer_core::rpc::OrchestratorToolInvocation],
        response: &str,
        turn_started_at: &Instant,
    ) {
        for invocation in invocations {
            let mut call_event = system0_event(
                "tool_call",
                turn_id,
                serde_json::json!({
                    "call_id": invocation.call_id,
                    "tool_id": invocation.tool_id,
                    "arguments": invocation.arguments,
                }),
            );
            call_event.turn_id = Some(turn_id.to_string());
            call_event.call_id = Some(invocation.call_id.clone());
            let _ = self.thread_journal.append_event(call_event).await;

            let mut result_event = system0_event(
                "tool_result",
                turn_id,
                serde_json::json!({
                    "call_id": invocation.call_id,
                    "tool_id": invocation.tool_id,
                    "status": invocation.status,
                    "output": invocation.output,
                }),
            );
            result_event.turn_id = Some(turn_id.to_string());
            result_event.call_id = Some(invocation.call_id.clone());
            let _ = self.thread_journal.append_event(result_event).await;
        }

        let mut event = system0_event(
            "message_assistant",
            turn_id,
            serde_json::json!({
                "role": "assistant", "content": response
            }),
        );
        event.turn_id = Some(turn_id.to_string());
        event.duration_ms = Some(turn_started_at.elapsed().as_millis() as u64);
        let _ = self.thread_journal.append_event(event).await;
    }
}

/// Build a `ThreadEvent` for the System0 thread with common fields pre-filled.
fn system0_event(event_type: &str, run_id: uuid::Uuid, payload: serde_json::Value) -> ThreadEvent {
    make_event(
        SYSTEM0_AGENT_ID,
        "system",
        event_type,
        Some(SYSTEM0_AGENT_ID.to_string()),
        Some(run_id.to_string()),
        payload,
    )
}
