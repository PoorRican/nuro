use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::argument_tokens::expand_user_query_tokens;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{DelegatedRun, OrchestratorOutputItem, ThreadSummary, TurnDelegation};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::state::{ActiveRunContext, SubAgentThreadState, System0ToolBroker};
use crate::orchestrator::tool_id::RuntimeToolId;
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, normalize_error_message,
};
use crate::orchestrator::tracing::thread_journal::{
    ThreadJournal, make_event, now_rfc3339, sanitize_thread_file_component,
};

impl System0ToolBroker {
    pub(crate) async fn handle_runtime_action(
        &self,
        id: RuntimeToolId,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.turn.current_turn_id;

        match id {
            RuntimeToolId::ListAgents => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "agents": inner.agents.subagents.keys().cloned().collect::<Vec<_>>()
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            RuntimeToolId::ReadConfig => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(inner.improvement.config_snapshot.clone()),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            RuntimeToolId::QueueStatus => {
                let mut queued = 0usize;
                let mut running = 0usize;
                let mut completed = 0usize;
                let mut failed = 0usize;
                for run in inner.runs.runs_index.values() {
                    match run.state.as_str() {
                        "queued" => queued += 1,
                        "running" => running += 1,
                        "completed" => completed += 1,
                        "failed" | "error" => failed += 1,
                        _ => {}
                    }
                }

                let runs = inner
                    .runs
                    .runs_order
                    .iter()
                    .filter_map(|run_id| inner.runs.runs_index.get(run_id))
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
                        "outputs_pending": inner.runs.pending_output_queue.len(),
                        "runs": runs,
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            RuntimeToolId::DelegateToAgent => {
                let resolved_arguments =
                    expand_user_query_tokens(&call.arguments, &inner.turn.current_turn_user_query);

                let Some(agent_id) = resolved_arguments
                    .get("agent_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "delegate_to_agent".to_string(),
                        message: "missing 'agent_id'".to_string(),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
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
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                };
                let instruction = instruction.to_string();

                let Some(runtime) = inner.agents.subagents.get(&agent_id).cloned() else {
                    let err = NeuromancerError::Tool(ToolError::NotFound {
                        tool_id: format!("delegate_to_agent:{agent_id}"),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                };

                let run_uuid = uuid::Uuid::new_v4();
                let run_id = run_uuid.to_string();
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
                inner
                    .threads
                    .thread_states
                    .insert(thread_id.clone(), thread_state);

                let run = DelegatedRun {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    state: "queued".to_string(),
                    summary: None,
                    thread_id: Some(thread_id.clone()),
                    initial_instruction: Some(instruction.clone()),
                    error: None,
                };
                inner.runs.runs_index.insert(run_id.clone(), run);
                if !inner.runs.runs_order.iter().any(|id| id == &run_id) {
                    inner.runs.runs_order.push(run_id.clone());
                }
                inner.active_runs_by_run_id.insert(
                    run_id.clone(),
                    ActiveRunContext {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        thread_id: thread_id.clone(),
                        turn_id: Some(turn_id.to_string()),
                        call_id: Some(call.id.clone()),
                    },
                );

                inner
                    .runs
                    .delegations_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(TurnDelegation {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        thread_id: thread_id.clone(),
                        state: "queued".to_string(),
                    });

                let session_store = inner.agents.session_store.clone();
                let thread_journal = inner.threads.thread_journal.clone();
                let current_trigger_type = inner.turn.current_trigger_type;
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
                inner
                    .runs
                    .record_invocation(turn_id, &call, &tool_result.output);
                drop(inner);

                let persisted_count_before_delta = journal_thread_created(
                    &thread_journal,
                    &thread_id,
                    &agent_id,
                    &run_id,
                    &instruction,
                    turn_id,
                    &call_id,
                )
                .await;

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
                        run_uuid,
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
        }
    }
}

/// Contextual state for a delegated task execution, threaded through sub-functions.
struct DelegationContext {
    thread_journal: ThreadJournal,
    turn_id: uuid::Uuid,
    call_id: String,
    run_id: String,
    thread_id: String,
    agent_id: String,
}

impl DelegationContext {
    /// Build a subagent thread event with common fields pre-filled.
    fn event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> neuromancer_core::rpc::ThreadEvent {
        let mut event = make_event(
            &self.thread_id,
            "subagent",
            event_type,
            Some(self.agent_id.clone()),
            Some(self.run_id.clone()),
            payload,
        );
        event.turn_id = Some(self.turn_id.to_string());
        event.call_id = Some(self.call_id.clone());
        event
    }

    fn thread_snapshot(&self, state: &str) -> ThreadSummary {
        ThreadSummary {
            thread_id: self.thread_id.clone(),
            kind: "subagent".to_string(),
            agent_id: Some(self.agent_id.clone()),
            latest_run_id: Some(self.run_id.clone()),
            state: state.to_string(),
            updated_at: now_rfc3339(),
            resurrected: false,
            active: true,
        }
    }

    async fn journal_snapshot_and_state(
        &self,
        state: &str,
        summary: Option<&str>,
        error: Option<&str>,
        duration_ms: Option<u64>,
    ) {
        if let Err(err) = self
            .thread_journal
            .append_index_snapshot(&self.thread_snapshot(state))
            .await
        {
            tracing::error!(thread_id = %self.thread_id, run_id = %self.run_id, error = ?err, "thread_journal_index_write_failed");
        }
        let mut event = self.event(
            "run_state_changed",
            serde_json::json!({
                "state": state,
                "summary": summary,
                "error": error,
            }),
        );
        event.duration_ms = duration_ms;
        if let Err(err) = self.thread_journal.append_event(event).await {
            tracing::error!(thread_id = %self.thread_id, run_id = %self.run_id, error = ?err, "thread_journal_state_write_failed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_delegated_task(
    broker: System0ToolBroker,
    runtime: std::sync::Arc<AgentRuntime>,
    session_store: InMemorySessionStore,
    thread_journal: ThreadJournal,
    turn_id: uuid::Uuid,
    current_trigger_type: TriggerType,
    call_id: String,
    run_task_id: uuid::Uuid,
    run_id: String,
    thread_id: String,
    agent_id: String,
    instruction: String,
    session_id: AgentSessionId,
    persisted_count_before_delta: usize,
) {
    let delegation_started_at = Instant::now();
    let ctx = DelegationContext {
        thread_journal,
        turn_id,
        call_id,
        run_id: run_id.clone(),
        thread_id: thread_id.clone(),
        agent_id: agent_id.clone(),
    };

    // --- Transition to running ---
    {
        let mut inner = broker.inner.lock().await;
        if let Some(run) = inner.runs.runs_index.get_mut(&run_id) {
            run.state = "running".to_string();
            run.summary = Some("Task started".to_string());
            run.error = None;
        }
        if let Some(state) = inner.threads.thread_states.get_mut(&thread_id) {
            state.state = "running".to_string();
            state.updated_at = now_rfc3339();
            state.active = true;
            state.latest_run_id = Some(run_id.clone());
        }
    }
    ctx.journal_snapshot_and_state("running", Some("Task started"), None, None)
        .await;
    tracing::info!(
        turn_id = %turn_id,
        run_id = %run_id,
        agent_id = %agent_id,
        thread_id = %thread_id,
        trigger_type = ?current_trigger_type,
        "delegation_started"
    );

    // --- Execute the agent turn ---
    let initial_instruction = instruction.clone();
    let result = runtime
        .execute_turn_with_task_id(
            &session_store,
            session_id,
            TriggerSource::Internal,
            instruction,
            run_task_id,
        )
        .await;

    // --- Process the result ---
    let (run_state, summary, error, full_output, persisted_message_count) =
        process_delegation_result(
            &ctx,
            &broker,
            result,
            &session_store,
            session_id,
            persisted_count_before_delta,
            &delegation_started_at,
        )
        .await;

    // --- Record final state ---
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
        inner.runs.runs_index.insert(run_id.clone(), run.clone());
        if !inner.runs.runs_order.iter().any(|id| id == &run_id) {
            inner.runs.runs_order.push(run_id.clone());
        }
        inner.active_runs_by_run_id.remove(&run_id);
        if let Some(state) = inner.threads.thread_states.get_mut(&thread_id) {
            state.latest_run_id = Some(run_id.clone());
            state.state = run_state.clone();
            state.summary = summary.clone();
            state.updated_at = now_rfc3339();
            state.persisted_message_count = persisted_message_count;
            state.active = true;
        }
    }

    let elapsed_ms = delegation_started_at.elapsed().as_millis() as u64;
    ctx.journal_snapshot_and_state(
        &run_state,
        summary.as_deref(),
        error.as_deref(),
        Some(elapsed_ms),
    )
    .await;

    let no_reply = full_output
        .as_deref()
        .is_some_and(|value| value.trim() == "NO_REPLY");
    broker
        .push_output(OrchestratorOutputItem {
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
        })
        .await;

    tracing::info!(
        turn_id = %turn_id, run_id = %run_id, agent_id = %agent_id,
        thread_id = %thread_id, state = %run.state,
        duration_ms = delegation_started_at.elapsed().as_millis(),
        "delegation_finished"
    );
}

async fn process_delegation_result(
    ctx: &DelegationContext,
    broker: &System0ToolBroker,
    result: Result<neuromancer_agent::runtime::TurnExecutionResult, NeuromancerError>,
    session_store: &InMemorySessionStore,
    session_id: AgentSessionId,
    persisted_count_before_delta: usize,
    started_at: &Instant,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    usize,
) {
    match result {
        Ok(turn_output) => {
            let response_text =
                crate::orchestrator::runtime::extract_response_text(&turn_output.output)
                    .unwrap_or_else(|| turn_output.output.summary.clone());
            let is_no_reply = response_text.trim() == "NO_REPLY";
            let full_output = Some(response_text.clone());

            let mut summary_text = turn_output.output.summary.clone();
            if summary_text.trim().is_empty() {
                summary_text = response_text.clone();
            }
            if is_no_reply {
                summary_text = "Task completed with NO_REPLY".to_string();
            }

            match session_store.get(session_id).await {
                Some(conversation) => {
                    let thread_messages =
                        conversation_to_thread_messages(&conversation.conversation.messages);
                    let delta = thread_messages
                        .iter()
                        .skip(persisted_count_before_delta)
                        .cloned()
                        .collect::<Vec<_>>();
                    if let Err(err) = ctx
                        .thread_journal
                        .append_messages(
                            &ctx.thread_id,
                            "subagent",
                            Some(&ctx.agent_id),
                            Some(&ctx.run_id),
                            &delta,
                        )
                        .await
                    {
                        tracing::error!(
                            thread_id = %ctx.thread_id, run_id = %ctx.run_id,
                            error = ?err, "thread_journal_delta_write_failed"
                        );
                    }
                    (
                        "completed".to_string(),
                        Some(summary_text),
                        None,
                        full_output,
                        thread_messages.len(),
                    )
                }
                None => {
                    let msg = format!(
                        "missing sub-agent conversation state for thread '{}'",
                        ctx.thread_id
                    );
                    (
                        "failed".to_string(),
                        Some(msg.clone()),
                        Some(msg),
                        full_output,
                        persisted_count_before_delta,
                    )
                }
            }
        }
        Err(err) => {
            let normalized = normalize_error_message(err.to_string());
            let enriched = enrich_delegate_error_message(broker, &ctx.run_id, normalized).await;
            let mut event = ctx.event(
                "error",
                serde_json::json!({
                    "error": enriched,
                    "tool_id": "delegate_to_agent",
                    "call_id": ctx.call_id,
                }),
            );
            event.duration_ms = Some(started_at.elapsed().as_millis() as u64);
            if let Err(journal_err) = ctx.thread_journal.append_event(event).await {
                tracing::error!(
                    thread_id = %ctx.thread_id, run_id = %ctx.run_id,
                    error = ?journal_err, "thread_journal_error_write_failed"
                );
            }
            (
                "failed".to_string(),
                Some(enriched.clone()),
                Some(enriched),
                None,
                persisted_count_before_delta,
            )
        }
    }
}

async fn enrich_delegate_error_message(
    broker: &System0ToolBroker,
    run_id: &str,
    base_error: String,
) -> String {
    let Some(snapshot) = broker.last_report_for_run(run_id).await else {
        return base_error;
    };
    let primary_reason = report_primary_reason(&snapshot.report_type, &snapshot.report)
        .unwrap_or_else(|| base_error.clone());
    let recommended_action = snapshot
        .recommended_remediation
        .as_ref()
        .and_then(remediation_action_name)
        .unwrap_or_else(|| "none".to_string());

    let suffix = serde_json::to_string(&serde_json::json!({
        "run_id": run_id,
        "report_type": snapshot.report_type,
        "reason": primary_reason,
        "recommended_action": recommended_action,
    }))
    .unwrap_or_else(|_| "{\"run_id\":\"unknown\"}".to_string());

    format!("{base_error} | remediation_context={suffix}")
}

fn report_primary_reason(report_type: &str, report: &serde_json::Value) -> Option<String> {
    match report_type {
        "stuck" => report
            .get("reason")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        "tool_failure" => report
            .get("error")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        "policy_denied" => {
            let code = report.get("policy_code").and_then(|value| value.as_str());
            let capability = report
                .get("capability_needed")
                .and_then(|value| value.as_str());
            match (code, capability) {
                (Some(code), Some(capability)) => Some(format!("{code}: {capability}")),
                (_, Some(capability)) => Some(capability.to_string()),
                _ => None,
            }
        }
        "input_required" => report
            .get("question")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        "failed" => report
            .get("error")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        _ => None,
    }
}

fn remediation_action_name(remediation: &serde_json::Value) -> Option<String> {
    if let Some(action) = remediation.get("action").and_then(|value| value.as_str()) {
        return Some(action.to_string());
    }
    // Backward compatibility with externally-tagged enum serialization.
    if remediation.get("Retry").is_some() {
        return Some("retry".to_string());
    }
    if remediation.get("Clarify").is_some() {
        return Some("clarify".to_string());
    }
    if remediation.get("GrantTemporary").is_some() {
        return Some("grant_temporary".to_string());
    }
    if remediation.get("Reassign").is_some() {
        return Some("reassign".to_string());
    }
    if remediation.get("EscalateToUser").is_some() {
        return Some("escalate_to_user".to_string());
    }
    if remediation.get("Abort").is_some() {
        return Some("abort".to_string());
    }
    None
}

async fn journal_thread_created(
    thread_journal: &ThreadJournal,
    thread_id: &str,
    agent_id: &str,
    run_id: &str,
    instruction: &str,
    turn_id: uuid::Uuid,
    call_id: &str,
) -> usize {
    let created = ThreadSummary {
        thread_id: thread_id.to_string(),
        kind: "subagent".to_string(),
        agent_id: Some(agent_id.to_string()),
        latest_run_id: Some(run_id.to_string()),
        state: "queued".to_string(),
        updated_at: now_rfc3339(),
        resurrected: false,
        active: true,
    };
    if let Err(err) = thread_journal.append_index_snapshot(&created).await {
        tracing::error!(thread_id = %thread_id, error = ?err, "thread_journal_index_write_failed");
    }

    let mut event = make_event(
        thread_id,
        "subagent",
        "thread_created",
        Some(agent_id.to_string()),
        Some(run_id.to_string()),
        serde_json::json!({ "agent_id": agent_id, "initial_instruction": instruction }),
    );
    event.turn_id = Some(turn_id.to_string());
    event.call_id = Some(call_id.to_string());
    if let Err(err) = thread_journal.append_event(event).await {
        tracing::error!(thread_id = %thread_id, error = ?err, "thread_journal_write_failed");
    }

    let mut event = make_event(
        thread_id,
        "subagent",
        "message_user",
        Some(agent_id.to_string()),
        Some(run_id.to_string()),
        serde_json::json!({ "role": "user", "content": instruction }),
    );
    event.turn_id = Some(turn_id.to_string());
    event.call_id = Some(call_id.to_string());
    match thread_journal.append_event(event).await {
        Ok(()) => 1,
        Err(err) => {
            tracing::error!(thread_id = %thread_id, error = ?err, "thread_journal_write_failed");
            0
        }
    }
}
