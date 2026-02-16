use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::argument_tokens::expand_user_query_tokens;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, DelegatedTask, OrchestratorOutputItem, ThreadEvent, ThreadSummary,
};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::state::{SubAgentThreadState, System0ToolBroker};
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, normalize_error_message,
};
use crate::orchestrator::tracing::thread_journal::{now_rfc3339, sanitize_thread_file_component};

pub const TOOL_IDS: &[&str] = &[
    "delegate_to_agent",
    "list_agents",
    "read_config",
    "queue_status",
];

pub fn contains(tool_id: &str) -> bool {
    TOOL_IDS.contains(&tool_id)
}

impl System0ToolBroker {
    pub(crate) async fn handle_runtime_action(
        &self,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.current_turn_id;

        match call.tool_id.as_str() {
            "list_agents" => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "agents": inner.subagents.keys().cloned().collect::<Vec<_>>()
                    })),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                Ok(result)
            }
            "read_config" => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(inner.config_snapshot.clone()),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                Ok(result)
            }
            "queue_status" => {
                let mut queued = 0usize;
                let mut running = 0usize;
                let mut completed = 0usize;
                let mut failed = 0usize;
                for run in inner.runs_index.values() {
                    match run.state.as_str() {
                        "queued" => queued += 1,
                        "running" => running += 1,
                        "completed" => completed += 1,
                        "failed" | "error" => failed += 1,
                        _ => {}
                    }
                }

                let runs = inner
                    .runs_order
                    .iter()
                    .filter_map(|run_id| inner.runs_index.get(run_id))
                    .map(|run| {
                        serde_json::json!({
                            "run_id": run.run_id,
                            "agent_id": run.agent_id,
                            "thread_id": run.thread_id,
                            "state": run.state,
                            "summary": run.summary,
                            "error": run.error,
                        })
                    })
                    .collect::<Vec<_>>();

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "queued": queued,
                        "running": running,
                        "completed": completed,
                        "failed": failed,
                        "outputs_pending": inner.pending_output_queue.len(),
                        "runs": runs,
                    })),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                Ok(result)
            }
            "delegate_to_agent" => {
                let resolved_arguments =
                    expand_user_query_tokens(&call.arguments, &inner.current_turn_user_query);

                let Some(agent_id) = resolved_arguments
                    .get("agent_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "delegate_to_agent".to_string(),
                        message: "missing 'agent_id'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };
                let agent_id = agent_id.to_string();

                let Some(instruction) = resolved_arguments
                    .get("instruction")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "delegate_to_agent".to_string(),
                        message: "missing 'instruction'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };
                let instruction = instruction.to_string();

                let Some(runtime) = inner.subagents.get(&agent_id).cloned() else {
                    let err = NeuromancerError::Tool(ToolError::NotFound {
                        tool_id: format!("delegate_to_agent:{agent_id}"),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };

                let run_id = uuid::Uuid::new_v4().to_string();
                let thread_id = format!(
                    "{}-{}",
                    sanitize_thread_file_component(&agent_id),
                    uuid::Uuid::new_v4()
                        .to_string()
                        .chars()
                        .take(8)
                        .collect::<String>()
                );
                let now = now_rfc3339();
                let session_id = uuid::Uuid::new_v4();

                let thread_state = SubAgentThreadState {
                    thread_id: thread_id.clone(),
                    agent_id: agent_id.clone(),
                    session_id,
                    latest_run_id: Some(run_id.clone()),
                    state: "queued".to_string(),
                    summary: None,
                    initial_instruction: Some(instruction.clone()),
                    resurrected: false,
                    active: true,
                    updated_at: now.clone(),
                    persisted_message_count: 0,
                };
                inner.thread_states.insert(thread_id.clone(), thread_state);

                let run = DelegatedRun {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    state: "queued".to_string(),
                    summary: None,
                    thread_id: Some(thread_id.clone()),
                    initial_instruction: Some(instruction.clone()),
                    error: None,
                };
                inner.runs_index.insert(run_id.clone(), run);
                if !inner.runs_order.iter().any(|id| id == &run_id) {
                    inner.runs_order.push(run_id.clone());
                }

                inner
                    .delegated_tasks_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(DelegatedTask {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        thread_id: thread_id.clone(),
                        state: "queued".to_string(),
                    });

                let session_store = inner.session_store.clone();
                let thread_journal = inner.thread_journal.clone();
                let current_trigger_type = inner.current_trigger_type;
                let call_id = call.id.clone();

                let tool_result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "run_id": run_id,
                        "thread_id": thread_id,
                        "agent_id": agent_id,
                        "state": "queued",
                    })),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &tool_result.output);
                drop(inner);

                let created = ThreadSummary {
                    thread_id: thread_id.clone(),
                    kind: "subagent".to_string(),
                    agent_id: Some(agent_id.clone()),
                    latest_run_id: Some(run_id.clone()),
                    state: "queued".to_string(),
                    updated_at: now_rfc3339(),
                    resurrected: false,
                    active: true,
                };
                if let Err(err) = thread_journal.append_index_snapshot(&created).await {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = ?err,
                        "thread_journal_index_write_failed"
                    );
                }
                if let Err(err) = thread_journal
                    .append_event(ThreadEvent {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        thread_id: thread_id.clone(),
                        thread_kind: "subagent".to_string(),
                        seq: 0,
                        ts: now_rfc3339(),
                        event_type: "thread_created".to_string(),
                        agent_id: Some(agent_id.clone()),
                        run_id: Some(run_id.clone()),
                        payload: serde_json::json!({
                            "agent_id": agent_id,
                            "initial_instruction": instruction,
                        }),
                        redaction_applied: false,
                        turn_id: Some(turn_id.to_string()),
                        parent_event_id: None,
                        call_id: Some(call_id.clone()),
                        attempt: None,
                        duration_ms: None,
                        meta: None,
                    })
                    .await
                {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = ?err,
                        "thread_journal_write_failed"
                    );
                }

                let mut persisted_count_before_delta = 0usize;
                if let Err(err) = thread_journal
                    .append_event(ThreadEvent {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        thread_id: thread_id.clone(),
                        thread_kind: "subagent".to_string(),
                        seq: 0,
                        ts: now_rfc3339(),
                        event_type: "message_user".to_string(),
                        agent_id: Some(agent_id.clone()),
                        run_id: Some(run_id.clone()),
                        payload: serde_json::json!({ "role": "user", "content": instruction.clone() }),
                        redaction_applied: false,
                        turn_id: Some(turn_id.to_string()),
                        parent_event_id: None,
                        call_id: Some(call_id.clone()),
                        attempt: None,
                        duration_ms: None,
                        meta: None,
                    })
                    .await
                {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = ?err,
                        "thread_journal_write_failed"
                    );
                } else {
                    persisted_count_before_delta = 1;
                }

                let broker = self.clone();
                tokio::spawn(async move {
                    run_delegated_task(
                        broker,
                        runtime,
                        session_store,
                        thread_journal,
                        turn_id,
                        current_trigger_type,
                        call_id,
                        run_id,
                        thread_id,
                        agent_id,
                        instruction,
                        session_id,
                        persisted_count_before_delta,
                    )
                    .await;
                });

                Ok(tool_result)
            }
            _ => {
                let err = Self::not_found_err(&call.tool_id);
                Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                Err(err)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_delegated_task(
    broker: System0ToolBroker,
    runtime: std::sync::Arc<AgentRuntime>,
    session_store: InMemorySessionStore,
    thread_journal: crate::orchestrator::tracing::thread_journal::ThreadJournal,
    turn_id: uuid::Uuid,
    current_trigger_type: TriggerType,
    call_id: String,
    run_id: String,
    thread_id: String,
    agent_id: String,
    instruction: String,
    session_id: AgentSessionId,
    persisted_count_before_delta: usize,
) {
    let delegation_started_at = Instant::now();

    {
        let mut inner = broker.inner.lock().await;
        if let Some(run) = inner.runs_index.get_mut(&run_id) {
            run.state = "running".to_string();
            run.summary = Some("Task started".to_string());
            run.error = None;
        }
        if let Some(state) = inner.thread_states.get_mut(&thread_id) {
            state.state = "running".to_string();
            state.updated_at = now_rfc3339();
            state.active = true;
            state.latest_run_id = Some(run_id.clone());
        }
    }

    let running_snapshot = ThreadSummary {
        thread_id: thread_id.clone(),
        kind: "subagent".to_string(),
        agent_id: Some(agent_id.clone()),
        latest_run_id: Some(run_id.clone()),
        state: "running".to_string(),
        updated_at: now_rfc3339(),
        resurrected: false,
        active: true,
    };
    if let Err(err) = thread_journal
        .append_index_snapshot(&running_snapshot)
        .await
    {
        tracing::error!(
            thread_id = %thread_id,
            run_id = %run_id,
            error = ?err,
            "thread_journal_index_write_failed"
        );
    }
    if let Err(err) = thread_journal
        .append_event(ThreadEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            thread_id: thread_id.clone(),
            thread_kind: "subagent".to_string(),
            seq: 0,
            ts: now_rfc3339(),
            event_type: "run_state_changed".to_string(),
            agent_id: Some(agent_id.clone()),
            run_id: Some(run_id.clone()),
            payload: serde_json::json!({
                "state": "running",
                "summary": "Task started",
                "error": serde_json::Value::Null,
            }),
            redaction_applied: false,
            turn_id: Some(turn_id.to_string()),
            parent_event_id: None,
            call_id: Some(call_id.clone()),
            attempt: None,
            duration_ms: None,
            meta: None,
        })
        .await
    {
        tracing::error!(
            thread_id = %thread_id,
            run_id = %run_id,
            error = ?err,
            "thread_journal_state_write_failed"
        );
    }

    tracing::info!(
        turn_id = %turn_id,
        run_id = %run_id,
        agent_id = %agent_id,
        thread_id = %thread_id,
        trigger_type = ?current_trigger_type,
        "delegation_started"
    );

    let initial_instruction = instruction.clone();
    let result = runtime
        .execute_turn(
            &session_store,
            session_id,
            TriggerSource::Internal,
            instruction,
        )
        .await;

    let mut error = None;
    let mut full_output = None::<String>;
    let mut summary: Option<String>;
    let mut persisted_message_count = persisted_count_before_delta;
    let mut run_state = "completed".to_string();

    match result {
        Ok(turn_output) => {
            let response_text =
                crate::orchestrator::runtime::extract_response_text(&turn_output.output)
                    .unwrap_or_else(|| turn_output.output.summary.clone());
            let is_no_reply = response_text.trim() == "NO_REPLY";
            full_output = Some(response_text.clone());

            let mut summary_text = turn_output.output.summary.clone();
            if summary_text.trim().is_empty() {
                summary_text = response_text.clone();
            }
            if is_no_reply {
                summary_text = "Task completed with NO_REPLY".to_string();
            }
            summary = Some(summary_text);

            match session_store.get(session_id).await {
                Some(conversation) => {
                    let thread_messages =
                        conversation_to_thread_messages(&conversation.conversation.messages);
                    let delta = thread_messages
                        .iter()
                        .skip(persisted_count_before_delta)
                        .cloned()
                        .collect::<Vec<_>>();
                    if let Err(err) = thread_journal
                        .append_messages(
                            &thread_id,
                            "subagent",
                            Some(&agent_id),
                            Some(&run_id),
                            &delta,
                        )
                        .await
                    {
                        tracing::error!(
                            thread_id = %thread_id,
                            run_id = %run_id,
                            error = ?err,
                            "thread_journal_delta_write_failed"
                        );
                    }
                    persisted_message_count = thread_messages.len();
                }
                None => {
                    let msg = format!(
                        "missing sub-agent conversation state for thread '{}'",
                        thread_id
                    );
                    error = Some(msg.clone());
                    summary = Some(msg);
                    run_state = "failed".to_string();
                }
            }
        }
        Err(err) => {
            let normalized = normalize_error_message(err.to_string());
            error = Some(normalized.clone());
            summary = Some(normalized.clone());
            run_state = "failed".to_string();

            if let Err(journal_err) = thread_journal
                .append_event(ThreadEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    thread_id: thread_id.clone(),
                    thread_kind: "subagent".to_string(),
                    seq: 0,
                    ts: now_rfc3339(),
                    event_type: "error".to_string(),
                    agent_id: Some(agent_id.clone()),
                    run_id: Some(run_id.clone()),
                    payload: serde_json::json!({
                        "error": normalized,
                        "tool_id": "delegate_to_agent",
                        "call_id": call_id.clone(),
                    }),
                    redaction_applied: false,
                    turn_id: Some(turn_id.to_string()),
                    parent_event_id: None,
                    call_id: Some(call_id.clone()),
                    attempt: None,
                    duration_ms: Some(delegation_started_at.elapsed().as_millis() as u64),
                    meta: None,
                })
                .await
            {
                tracing::error!(
                    thread_id = %thread_id,
                    run_id = %run_id,
                    error = ?journal_err,
                    "thread_journal_error_write_failed"
                );
            }
        }
    }

    let run = DelegatedRun {
        run_id: run_id.clone(),
        agent_id: agent_id.clone(),
        state: run_state.clone(),
        summary: summary.clone(),
        thread_id: Some(thread_id.clone()),
        initial_instruction: Some(initial_instruction),
        error: error.clone(),
    };

    {
        let mut inner = broker.inner.lock().await;
        inner.runs_index.insert(run_id.clone(), run.clone());
        if !inner.runs_order.iter().any(|id| id == &run_id) {
            inner.runs_order.push(run_id.clone());
        }

        if let Some(state) = inner.thread_states.get_mut(&thread_id) {
            state.latest_run_id = Some(run_id.clone());
            state.state = run_state.clone();
            state.summary = summary.clone();
            state.updated_at = now_rfc3339();
            state.persisted_message_count = persisted_message_count;
            state.active = true;
        }
    }

    let completed_snapshot = ThreadSummary {
        thread_id: thread_id.clone(),
        kind: "subagent".to_string(),
        agent_id: Some(agent_id.clone()),
        latest_run_id: Some(run_id.clone()),
        state: run_state.clone(),
        updated_at: now_rfc3339(),
        resurrected: false,
        active: true,
    };
    if let Err(err) = thread_journal
        .append_index_snapshot(&completed_snapshot)
        .await
    {
        tracing::error!(
            thread_id = %thread_id,
            run_id = %run_id,
            error = ?err,
            "thread_journal_index_write_failed"
        );
    }
    if let Err(err) = thread_journal
        .append_event(ThreadEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            thread_id: thread_id.clone(),
            thread_kind: "subagent".to_string(),
            seq: 0,
            ts: now_rfc3339(),
            event_type: "run_state_changed".to_string(),
            agent_id: Some(agent_id.clone()),
            run_id: Some(run_id.clone()),
            payload: serde_json::json!({
                "state": run_state,
                "summary": summary,
                "error": error,
            }),
            redaction_applied: false,
            turn_id: Some(turn_id.to_string()),
            parent_event_id: None,
            call_id: Some(call_id.clone()),
            attempt: None,
            duration_ms: Some(delegation_started_at.elapsed().as_millis() as u64),
            meta: None,
        })
        .await
    {
        tracing::error!(
            thread_id = %thread_id,
            run_id = %run_id,
            error = ?err,
            "thread_journal_state_write_failed"
        );
    }

    let no_reply = full_output
        .as_deref()
        .is_some_and(|value| value.trim() == "NO_REPLY");
    let output_item = OrchestratorOutputItem {
        output_id: uuid::Uuid::new_v4().to_string(),
        run_id: run_id.clone(),
        thread_id: thread_id.clone(),
        agent_id: agent_id.clone(),
        state: run.state.clone(),
        summary: run.summary.clone(),
        error: run.error.clone(),
        no_reply,
        content: if run.state == "completed" {
            if no_reply {
                Some("I've finished.".to_string())
            } else {
                full_output
            }
        } else {
            None
        },
        published_at: now_rfc3339(),
    };
    broker.push_output(output_item).await;

    tracing::info!(
        turn_id = %turn_id,
        run_id = %run_id,
        agent_id = %agent_id,
        thread_id = %thread_id,
        state = %run.state,
        duration_ms = delegation_started_at.elapsed().as_millis(),
        "delegation_finished"
    );
}
