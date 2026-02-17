use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{DelegatedRun, ThreadEvent, ThreadSummary};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::TriggerSource;
use std::time::{Duration, Instant};

use crate::orchestrator::state::{
    ActiveRunContext, RunReportSnapshot, SubAgentThreadState, System0ToolBroker,
};
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, normalize_error_message,
};
use crate::orchestrator::tracing::thread_journal::{now_rfc3339, sanitize_thread_file_component};

pub const TOOL_IDS: &[&str] = &["delegate_to_agent", "list_agents", "read_config"];

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
            "delegate_to_agent" => {
                let Some(agent_id) = call
                    .arguments
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

                let Some(instruction) = call
                    .arguments
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

                let run_uuid = uuid::Uuid::new_v4();
                let run_id = run_uuid.to_string();
                let now = now_rfc3339();
                let thread_id = inner
                    .thread_id_by_agent
                    .get(&agent_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        format!(
                            "{}-{}",
                            sanitize_thread_file_component(&agent_id),
                            uuid::Uuid::new_v4()
                                .to_string()
                                .chars()
                                .take(8)
                                .collect::<String>()
                        )
                    });
                let mut is_new_thread = false;
                let state_entry =
                    inner
                        .thread_states
                        .entry(thread_id.clone())
                        .or_insert_with(|| {
                            is_new_thread = true;
                            SubAgentThreadState {
                                thread_id: thread_id.clone(),
                                agent_id: agent_id.clone(),
                                session_id: uuid::Uuid::new_v4(),
                                latest_run_id: None,
                                state: "running".to_string(),
                                summary: None,
                                initial_instruction: Some(instruction.clone()),
                                resurrected: false,
                                active: true,
                                updated_at: now.clone(),
                                persisted_message_count: 0,
                            }
                        });

                if state_entry.initial_instruction.is_none() {
                    state_entry.initial_instruction = Some(instruction.clone());
                }
                state_entry.active = true;
                state_entry.state = "running".to_string();
                state_entry.updated_at = now.clone();
                state_entry.latest_run_id = Some(run_id.clone());
                let session_id = state_entry.session_id;
                let persisted_count_before_turn = state_entry.persisted_message_count;
                let initial_instruction = state_entry.initial_instruction.clone();

                inner
                    .thread_id_by_agent
                    .insert(agent_id.clone(), thread_id.clone());
                inner
                    .running_agents
                    .insert(agent_id.clone(), run_id.clone());
                inner.runs_index.insert(
                    run_id.clone(),
                    DelegatedRun {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        state: "running".to_string(),
                        summary: None,
                        thread_id: Some(thread_id.clone()),
                        initial_instruction: initial_instruction.clone(),
                        error: None,
                    },
                );
                if !inner.runs_order.iter().any(|id| id == &run_id) {
                    inner.runs_order.push(run_id.clone());
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

                let session_store = inner.session_store.clone();
                let thread_journal = inner.thread_journal.clone();
                let current_trigger_type = inner.current_trigger_type;
                drop(inner);

                if is_new_thread {
                    let created = ThreadSummary {
                        thread_id: thread_id.clone(),
                        kind: "subagent".to_string(),
                        agent_id: Some(agent_id.clone()),
                        latest_run_id: Some(run_id.clone()),
                        state: "running".to_string(),
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
                                "agent_id": agent_id.clone(),
                                "initial_instruction": instruction.clone(),
                            }),
                            redaction_applied: false,
                            turn_id: Some(turn_id.to_string()),
                            parent_event_id: None,
                            call_id: Some(call.id.clone()),
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
                }

                let mut persisted_count_before_delta = persisted_count_before_turn;
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
                        call_id: Some(call.id.clone()),
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
                    persisted_count_before_delta = persisted_count_before_delta.saturating_add(1);
                }

                let delegation_started_at = Instant::now();
                tracing::info!(
                    turn_id = %turn_id,
                    run_id = %run_id,
                    agent_id = %agent_id,
                    thread_id = %thread_id,
                    tool_id = %call.tool_id,
                    trigger_type = ?current_trigger_type,
                    "delegation_started"
                );

                let result = runtime
                    .execute_turn_with_task_id(
                        &session_store,
                        session_id,
                        TriggerSource::Internal,
                        instruction.clone(),
                        run_uuid,
                    )
                    .await;

                let mut error = None;
                let mut persisted_message_count = persisted_count_before_delta;
                let (run_state, summary) = match result {
                    Ok(turn_output) => {
                        let response = crate::orchestrator::runtime::extract_response_text(
                            &turn_output.output,
                        )
                        .unwrap_or_else(|| turn_output.output.summary.clone());
                        let mut run_state = "completed".to_string();
                        let mut summary = Some(response);

                        match session_store.get(session_id).await {
                            Some(conversation) => {
                                let thread_messages = conversation_to_thread_messages(
                                    &conversation.conversation.messages,
                                );
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
                                error = Some(format!(
                                    "missing sub-agent conversation state for thread '{}'",
                                    thread_id
                                ));
                                run_state = "failed".to_string();
                                summary = error.clone();
                            }
                        }
                        (run_state, summary)
                    }
                    Err(err) => {
                        let normalized = normalize_error_message(err.to_string());
                        error = Some(normalized.clone());
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
                                    "call_id": call.id.clone(),
                                }),
                                redaction_applied: false,
                                turn_id: Some(turn_id.to_string()),
                                parent_event_id: None,
                                call_id: Some(call.id.clone()),
                                attempt: None,
                                duration_ms: Some(
                                    delegation_started_at.elapsed().as_millis() as u64
                                ),
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
                        ("failed".to_string(), error.clone())
                    }
                };

                let run = DelegatedRun {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    state: run_state.clone(),
                    summary: summary.clone(),
                    thread_id: Some(thread_id.clone()),
                    initial_instruction: initial_instruction.clone(),
                    error: error.clone(),
                };
                let remediation_snapshot = if run_state == "failed" {
                    self.await_report_snapshot_for_run(&run_id, Duration::from_millis(400))
                        .await
                } else {
                    None
                };

                let mut thread_summary = None::<ThreadSummary>;
                let mut inner = self.inner.lock().await;
                inner.running_agents.remove(&agent_id);
                inner
                    .runs_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(run.clone());
                inner.runs_index.insert(run.run_id.clone(), run.clone());
                if !inner.runs_order.iter().any(|id| id == &run.run_id) {
                    inner.runs_order.push(run.run_id.clone());
                }
                if let Some(state) = inner.thread_states.get_mut(&thread_id) {
                    state.latest_run_id = Some(run_id.clone());
                    state.state = run_state.clone();
                    state.summary = summary.clone();
                    if state.initial_instruction.is_none() {
                        state.initial_instruction = initial_instruction.clone();
                    }
                    state.updated_at = now_rfc3339();
                    state.persisted_message_count = persisted_message_count;
                    state.active = true;
                    thread_summary = Some(ThreadSummary {
                        thread_id: thread_id.clone(),
                        kind: "subagent".to_string(),
                        agent_id: Some(agent_id.clone()),
                        latest_run_id: Some(run_id.clone()),
                        state: state.state.clone(),
                        updated_at: state.updated_at.clone(),
                        resurrected: state.resurrected,
                        active: state.active,
                    });
                }

                let tool_output = if run_state == "failed" {
                    ToolOutput::Error(build_delegate_error_text(
                        run.error
                            .clone()
                            .unwrap_or_else(|| "delegation failed".to_string()),
                        &run_id,
                        remediation_snapshot.as_ref(),
                    ))
                } else {
                    ToolOutput::Success(serde_json::json!({
                        "run_id": run.run_id,
                        "thread_id": thread_id,
                        "agent_id": run.agent_id,
                        "state": run.state,
                        "summary": run.summary,
                        "error": run.error,
                    }))
                };
                let tool_result = ToolResult {
                    call_id: call.id.clone(),
                    output: tool_output.clone(),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &tool_result.output);
                drop(inner);

                if let Some(snapshot) = thread_summary
                    && let Err(err) = thread_journal.append_index_snapshot(&snapshot).await
                {
                    tracing::error!(
                        thread_id = %snapshot.thread_id,
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
                        call_id: Some(call.id.clone()),
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

                tracing::info!(
                    turn_id = %turn_id,
                    run_id = %run_id,
                    agent_id = %agent_id,
                    thread_id = %thread_id,
                    state = %run.state,
                    duration_ms = delegation_started_at.elapsed().as_millis(),
                    "delegation_finished"
                );

                Ok(tool_result)
            }
            _ => {
                let err = Self::not_found_err(&call.tool_id);
                Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                Err(err)
            }
        }
    }

    async fn await_report_snapshot_for_run(
        &self,
        run_id: &str,
        timeout: Duration,
    ) -> Option<RunReportSnapshot> {
        let started = Instant::now();
        let poll_interval = Duration::from_millis(40);

        loop {
            if let Some(snapshot) = self.last_report_for_run(run_id).await {
                return Some(snapshot);
            }
            if started.elapsed() >= timeout {
                return None;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

fn build_delegate_error_text(
    base_error: String,
    run_id: &str,
    snapshot: Option<&RunReportSnapshot>,
) -> String {
    let report_type = snapshot.map(|value| value.report_type.clone());
    let primary_reason = snapshot
        .and_then(|value| extract_report_reason(&value.report))
        .or_else(|| snapshot.and_then(|value| value.recommended_reason.clone()))
        .unwrap_or_else(|| base_error.clone());
    let recommended_action = snapshot.and_then(|value| value.recommended_action.clone());
    let context = serde_json::json!({
        "run_id": run_id,
        "report_type": report_type,
        "primary_reason": primary_reason,
        "recommended_action": recommended_action,
        "recommended_remediation": snapshot.and_then(|value| value.recommended_remediation.clone()),
    });
    format!("{base_error} | remediation_context={context}")
}

fn extract_report_reason(report: &serde_json::Value) -> Option<String> {
    for key in ["reason", "error", "question", "description", "summary"] {
        if let Some(value) = report.get(key).and_then(|value| value.as_str())
            && !value.trim().is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}
