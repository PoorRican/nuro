use std::sync::Arc;
use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::rpc::{OrchestratorTurnResult, ThreadEvent};
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::error::System0Error;
use crate::orchestrator::state::{SYSTEM0_AGENT_ID, System0ToolBroker};
use crate::orchestrator::tracing::conversation_projection::normalize_error_message;
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, make_event};

use super::extract_response_text;

pub(super) struct System0TurnWorker {
    pub(super) agent_runtime: Arc<AgentRuntime>,
    pub(super) session_store: InMemorySessionStore,
    pub(super) session_id: AgentSessionId,
    pub(super) system0_broker: System0ToolBroker,
    pub(super) thread_journal: ThreadJournal,
}

impl System0TurnWorker {
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

        let mut event = system0_event("message_user", turn_id, serde_json::json!({
            "role": "user", "content": message.clone()
        }));
        event.turn_id = Some(turn_id.to_string());
        let _ = self.thread_journal.append_event(event).await;

        let output = match self
            .agent_runtime
            .execute_turn(
                &self.session_store,
                self.session_id,
                TriggerSource::Cli,
                message.clone(),
            )
            .await
        {
            Ok(output) => output,
            Err(err) => {
                let normalized = normalize_error_message(err.to_string());
                let mut event = system0_event("error", turn_id, serde_json::json!({
                    "error": normalized,
                    "message": "orchestrator turn failed",
                }));
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

    async fn journal_turn_events(
        &self,
        turn_id: uuid::Uuid,
        invocations: &[neuromancer_core::rpc::OrchestratorToolInvocation],
        response: &str,
        turn_started_at: &Instant,
    ) {
        for invocation in invocations {
            let mut call_event = system0_event("tool_call", turn_id, serde_json::json!({
                "call_id": invocation.call_id,
                "tool_id": invocation.tool_id,
                "arguments": invocation.arguments,
            }));
            call_event.turn_id = Some(turn_id.to_string());
            call_event.call_id = Some(invocation.call_id.clone());
            let _ = self.thread_journal.append_event(call_event).await;

            let mut result_event = system0_event("tool_result", turn_id, serde_json::json!({
                "call_id": invocation.call_id,
                "tool_id": invocation.tool_id,
                "status": invocation.status,
                "output": invocation.output,
            }));
            result_event.turn_id = Some(turn_id.to_string());
            result_event.call_id = Some(invocation.call_id.clone());
            let _ = self.thread_journal.append_event(result_event).await;
        }

        let mut event = system0_event("message_assistant", turn_id, serde_json::json!({
            "role": "assistant", "content": response
        }));
        event.turn_id = Some(turn_id.to_string());
        event.duration_ms = Some(turn_started_at.elapsed().as_millis() as u64);
        let _ = self.thread_journal.append_event(event).await;
    }
}

/// Build a `ThreadEvent` for the System0 thread with common fields pre-filled.
fn system0_event(
    event_type: &str,
    run_id: uuid::Uuid,
    payload: serde_json::Value,
) -> ThreadEvent {
    make_event(
        SYSTEM0_AGENT_ID,
        "system",
        event_type,
        Some(SYSTEM0_AGENT_ID.to_string()),
        Some(run_id.to_string()),
        payload,
    )
}
