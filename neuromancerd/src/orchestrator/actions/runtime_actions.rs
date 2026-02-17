use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::argument_tokens::expand_user_query_tokens;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{DelegatedRun, OrchestratorOutputItem, ThreadSummary, TurnDelegation};
use neuromancer_core::task::{Artifact, ArtifactKind, Task, TaskOutput, TaskState, TokenUsage};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::state::{
    ActiveRunContext, SubAgentThreadState, System0ToolBroker, TaskExecutionContext,
    task_manager::running_state, task_manager::task_failure_payload,
};
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
                let tasks = inner.tasks.clone();
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
                let outputs_pending = inner.runs.pending_output_queue.len();
                drop(inner);

                let (queued, running, completed, failed, tasks_total) =
                    tasks.queue_snapshot().await;

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "queued": queued,
                        "running": running,
                        "completed": completed,
                        "failed": failed,
                        "tasks_total": tasks_total,
                        "outputs_pending": outputs_pending,
                        "runs": runs,
                    })),
                };
                let mut inner = self.inner.lock().await;
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

                if !inner.agents.subagents.contains_key(&agent_id) {
                    let err = NeuromancerError::Tool(ToolError::NotFound {
                        tool_id: format!("delegate_to_agent:{agent_id}"),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                }

                let task = Task::new(
                    TriggerSource::Internal,
                    instruction.clone(),
                    agent_id.clone(),
                );
                let task_id = task.id;
                let run_id = task_id.to_string();
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
                let current_trigger_type = inner.turn.current_trigger_type;
                let thread_journal = inner.threads.thread_journal.clone();
                let call_id = call.id.clone();
                let tasks = inner.tasks.clone();
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

                tasks
                    .register_execution_context(
                        task_id,
                        TaskExecutionContext {
                            turn_id: Some(turn_id),
                            call_id: Some(call_id.clone()),
                            thread_id: Some(thread_id.clone()),
                            trigger_type: current_trigger_type,
                            session_id: Some(session_id),
                            persisted_message_count: persisted_count_before_delta,
                            publish_output: true,
                        },
                    )
                    .await;

                let enqueue_result = tasks.enqueue_direct(task).await;
                let mut inner = self.inner.lock().await;
                if let Err(err) = enqueue_result {
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                }

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

                Ok(tool_result)
            }
        }
    }
}

pub(crate) async fn execute_queued_task(
    broker: System0ToolBroker,
    task: Task,
) -> Result<(), NeuromancerError> {
    let context = broker
        .task_execution_context(task.id)
        .await
        .unwrap_or(TaskExecutionContext {
            turn_id: None,
            call_id: None,
            thread_id: None,
            trigger_type: TriggerType::Internal,
            session_id: None,
            persisted_message_count: 0,
            publish_output: false,
        });
    let run_id = task.id.to_string();
    let thread_id = context.thread_id.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            sanitize_thread_file_component(&task.assigned_agent),
            task.id.to_string().chars().take(8).collect::<String>()
        )
    });

    let Some(runtime) = broker.runtime_for_agent(&task.assigned_agent).await else {
        let failure_message = format!("missing runtime for agent '{}'", task.assigned_agent);
        let _ = broker
            .transition_task(
                task.id,
                Some("running"),
                TaskState::Failed {
                    error: task_failure_payload(failure_message.clone()),
                },
                None,
            )
            .await;
        if context.publish_output {
            broker
                .push_output(OrchestratorOutputItem {
                    output_id: uuid::Uuid::new_v4().to_string(),
                    run_id,
                    thread_id,
                    agent_id: task.assigned_agent.clone(),
                    state: "failed".to_string(),
                    summary: Some(failure_message.clone()),
                    error: Some(failure_message),
                    no_reply: false,
                    content: None,
                    published_at: now_rfc3339(),
                })
                .await;
        }
        return Ok(());
    };

    let thread_journal = {
        let inner = broker.inner.lock().await;
        inner.threads.thread_journal.clone()
    };
    let session_store = broker.session_store().await;

    run_delegated_task(
        broker,
        runtime,
        session_store,
        thread_journal,
        context.turn_id.unwrap_or_else(uuid::Uuid::new_v4),
        context.trigger_type,
        context.call_id.unwrap_or_else(|| "task-worker".to_string()),
        run_id,
        thread_id,
        task.assigned_agent,
        task.instruction,
        context.session_id.unwrap_or_else(uuid::Uuid::new_v4),
        context.persisted_message_count,
        context.publish_output,
        task.id,
    )
    .await;

    Ok(())
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
    run_id: String,
    thread_id: String,
    agent_id: String,
    instruction: String,
    session_id: AgentSessionId,
    persisted_count_before_delta: usize,
    publish_output: bool,
    task_id: neuromancer_core::task::TaskId,
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
    if let Err(err) = broker
        .transition_task(
            task_id,
            Some("running"),
            running_state(neuromancer_core::agent::TaskExecutionState::Thinking {
                conversation_len: 0,
                iteration: 0,
            }),
            None,
        )
        .await
    {
        tracing::error!(task_id = %task_id, error = ?err, "task_transition_failed");
    }
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
            task_id,
        )
        .await;

    // --- Process the result ---
    let (run_state, summary, error, full_output, persisted_message_count, task_output) =
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

    let final_task_state = if run_state == "completed" {
        let output = task_output.unwrap_or_else(|| {
            build_fallback_task_output(
                full_output
                    .clone()
                    .unwrap_or_else(|| "Task completed".to_string()),
                summary
                    .clone()
                    .unwrap_or_else(|| "Task completed".to_string()),
                delegation_started_at.elapsed(),
            )
        });
        TaskState::Completed { output }
    } else {
        TaskState::Failed {
            error: task_failure_payload(
                error
                    .clone()
                    .unwrap_or_else(|| "delegated task failed".to_string()),
            ),
        }
    };
    if let Err(err) = broker
        .transition_task(task_id, Some("running"), final_task_state, None)
        .await
    {
        tracing::error!(task_id = %task_id, error = ?err, "task_transition_failed");
    }

    if publish_output {
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
    }

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
    Option<TaskOutput>,
) {
    match result {
        Ok(turn_output) => {
            let output = turn_output.output.clone();
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
                        Some(output),
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
                        Some(output),
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
                None,
            )
        }
    }
}

fn build_fallback_task_output(
    content: String,
    summary: String,
    duration: std::time::Duration,
) -> TaskOutput {
    TaskOutput {
        artifacts: vec![Artifact {
            kind: ArtifactKind::Text,
            name: "response".to_string(),
            content,
            mime_type: Some("text/plain".to_string()),
        }],
        summary,
        token_usage: TokenUsage::default(),
        duration,
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
