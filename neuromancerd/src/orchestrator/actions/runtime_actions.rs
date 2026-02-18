use std::time::Instant;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_core::argument_tokens::expand_user_query_tokens;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{DelegatedRun, OrchestratorOutputItem, ThreadSummary, TurnDelegation};
use neuromancer_core::task::{
    AgentOutput, Artifact, ArtifactKind, Task, TaskOutput, TaskState, TokenUsage,
};
use neuromancer_core::thread::{AgentThread, CompactionPolicy, ThreadScope, ThreadStatus, ThreadStore, TruncationStrategy};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::{TriggerSource, TriggerType};

use crate::orchestrator::state::{
    ActiveRunContext, SubAgentThreadState, System0ToolBroker, TaskExecutionContext,
    task_manager::running_state, task_manager::task_failure_payload,
};
use crate::orchestrator::tool_id::RuntimeToolId;
use crate::orchestrator::tracing::conversation_projection::normalize_error_message;
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
                let current_trigger_type = inner.turn.current_trigger_type;
                let thread_journal = inner.threads.thread_journal.clone();
                let thread_store = inner.agents.thread_store.clone();
                let call_id = call.id.clone();
                let tasks = inner.tasks.clone();
                drop(inner);

                // Create AgentThread in the thread store for this delegation.
                let chrono_now = chrono::Utc::now();
                let agent_thread = AgentThread {
                    id: thread_id.clone(),
                    agent_id: agent_id.clone(),
                    scope: ThreadScope::Task {
                        task_id: task_id.to_string(),
                    },
                    compaction_policy: CompactionPolicy::InPlace {
                        strategy: TruncationStrategy::default(),
                    },
                    context_window_budget: 128_000,
                    status: ThreadStatus::Active,
                    created_at: chrono_now,
                    updated_at: chrono_now,
                };
                if let Err(err) = thread_store.create_thread(&agent_thread).await {
                    tracing::error!(thread_id = %thread_id, error = ?err, "thread_store_create_failed");
                }

                journal_thread_created(
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
                    latest_run_id: Some(run_id.clone()),
                    state: "queued".to_string(),
                    summary: None,
                    initial_instruction: Some(instruction.clone()),
                    resurrected: false,
                    active: true,
                    updated_at: now.clone(),
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

    let (thread_journal, thread_store) = {
        let inner = broker.inner.lock().await;
        (
            inner.threads.thread_journal.clone(),
            inner.agents.thread_store.clone(),
        )
    };

    run_delegated_task(
        broker,
        runtime,
        thread_store,
        thread_journal,
        context.turn_id.unwrap_or_else(uuid::Uuid::new_v4),
        context.trigger_type,
        context.call_id.unwrap_or_else(|| "task-worker".to_string()),
        run_id,
        thread_id,
        task.assigned_agent,
        task.instruction,
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
    thread_store: std::sync::Arc<dyn ThreadStore>,
    thread_journal: ThreadJournal,
    turn_id: uuid::Uuid,
    current_trigger_type: TriggerType,
    call_id: String,
    run_id: String,
    thread_id: String,
    agent_id: String,
    instruction: String,
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
    #[allow(deprecated)]
    let result = runtime
        .execute_turn_with_thread_store(
            &*thread_store,
            &thread_id,
            TriggerSource::Internal,
            instruction,
            task_id,
            vec![],
        )
        .await;

    // --- Optional extraction step ---
    // Try to extract structured TaskOutput from the agent's raw output via an
    // additional LLM turn. Falls back to AgentOutput::into_task_output() on failure.
    let extracted_output = match &result {
        Ok(turn_output) => {
            try_extract_task_output(
                &*runtime,
                &*thread_store,
                &thread_id,
                task_id,
                &turn_output.output,
            )
            .await
        }
        Err(_) => None,
    };

    // --- Process the result ---
    let (run_state, summary, error, full_output, task_output) =
        process_delegation_result(
            &ctx,
            &broker,
            result,
            &delegation_started_at,
            extracted_output,
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

const EXTRACTION_PROMPT: &str = r#"Summarize the work you just completed into a structured output. Respond ONLY with a valid JSON object in this exact format (no markdown fencing, no extra text):
{"summary":"<brief 1-2 sentence summary>","artifacts":[{"kind":"text","name":"response","content":"<your primary response text>"}]}
If you produced code, use kind "code". If you produced a URL, use kind "url". Include all relevant output items as separate artifacts."#;

/// Run an extraction turn to convert an AgentOutput into a structured TaskOutput.
///
/// Injects an extraction prompt as a user message into the agent's thread and runs one
/// more LLM turn. The agent responds with structured JSON which is parsed into TaskOutput.
/// Returns None if extraction fails (caller should fall back to `into_task_output()`).
async fn try_extract_task_output(
    runtime: &AgentRuntime,
    thread_store: &dyn ThreadStore,
    thread_id: &str,
    task_id: neuromancer_core::task::TaskId,
    agent_output: &AgentOutput,
) -> Option<TaskOutput> {
    let extraction_id = uuid::Uuid::new_v4();
    let thread_id_owned = thread_id.to_string();
    #[allow(deprecated)]
    let extraction_result = runtime
        .execute_turn_with_thread_store(
            thread_store,
            &thread_id_owned,
            TriggerSource::Internal,
            EXTRACTION_PROMPT.to_string(),
            extraction_id,
            vec![],
        )
        .await;

    match extraction_result {
        Ok(extraction) => {
            let parsed = parse_extraction_response(&extraction.output.message, agent_output);
            if parsed.is_none() {
                tracing::debug!(
                    task_id = %task_id,
                    "extraction_parse_failed_using_fallback"
                );
            }
            parsed
        }
        Err(err) => {
            tracing::warn!(
                task_id = %task_id,
                error = ?err,
                "extraction_turn_failed"
            );
            None
        }
    }
}

/// Parse a structured extraction response into a TaskOutput.
///
/// Accepts raw JSON or JSON wrapped in markdown code fences. Falls back to None
/// if the response can't be parsed, letting the caller use `AgentOutput::into_task_output()`.
fn parse_extraction_response(message: &str, fallback: &AgentOutput) -> Option<TaskOutput> {
    let json_str = message.trim();

    // Strip optional markdown code fences
    let json_str = if json_str.starts_with("```") {
        json_str
            .strip_prefix("```json")
            .or_else(|| json_str.strip_prefix("```"))
            .and_then(|s| s.rsplit_once("```").map(|(content, _)| content))
            .unwrap_or(json_str)
            .trim()
    } else {
        json_str
    };

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let summary = parsed.get("summary")?.as_str()?.to_string();
    let artifacts_json = parsed.get("artifacts")?.as_array()?;

    let mut artifacts = Vec::new();
    for a in artifacts_json {
        let content = a.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.is_empty() {
            continue;
        }
        artifacts.push(Artifact {
            kind: match a.get("kind").and_then(|v| v.as_str()).unwrap_or("text") {
                "code" => ArtifactKind::Code,
                "file" => ArtifactKind::File,
                "url" => ArtifactKind::Url,
                "data" => ArtifactKind::Data,
                _ => ArtifactKind::Text,
            },
            name: a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("response")
                .to_string(),
            content: content.to_string(),
            mime_type: a.get("mime_type").and_then(|v| v.as_str()).map(String::from),
        });
    }

    if artifacts.is_empty() {
        return None;
    }

    Some(TaskOutput {
        summary,
        artifacts,
        token_usage: fallback.token_usage.clone(),
        duration: fallback.duration,
    })
}

async fn process_delegation_result(
    ctx: &DelegationContext,
    broker: &System0ToolBroker,
    result: Result<neuromancer_agent::runtime::TurnExecutionResult, NeuromancerError>,
    started_at: &Instant,
    extracted_output: Option<TaskOutput>,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<TaskOutput>,
) {
    match result {
        Ok(turn_output) => {
            let response_text = turn_output.output.message.clone();
            let is_no_reply = response_text.trim() == "NO_REPLY";
            let task_output =
                extracted_output.unwrap_or_else(|| turn_output.output.into_task_output());

            let mut summary_text = task_output.summary.clone();
            if summary_text.trim().is_empty() {
                summary_text = response_text.clone();
            }
            if is_no_reply {
                summary_text = "Task completed with NO_REPLY".to_string();
            }

            // Messages are already persisted by execute_turn_with_thread_store.
            (
                "completed".to_string(),
                Some(summary_text),
                None,
                Some(response_text),
                Some(task_output),
            )
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
) {
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
    if let Err(err) = thread_journal.append_event(event).await {
        tracing::error!(thread_id = %thread_id, error = ?err, "thread_journal_write_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_agent_output() -> AgentOutput {
        AgentOutput {
            message: "The answer is 42.".to_string(),
            token_usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            },
            duration: Duration::from_millis(500),
        }
    }

    #[test]
    fn parse_extraction_valid_json() {
        let fallback = sample_agent_output();
        let response = r#"{"summary":"Computed the answer","artifacts":[{"kind":"text","name":"response","content":"The answer is 42."}]}"#;

        let result = parse_extraction_response(response, &fallback).unwrap();
        assert_eq!(result.summary, "Computed the answer");
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].content, "The answer is 42.");
        assert_eq!(result.token_usage.total_tokens, 150);
    }

    #[test]
    fn parse_extraction_markdown_fenced_json() {
        let fallback = sample_agent_output();
        let response = "```json\n{\"summary\":\"Done\",\"artifacts\":[{\"kind\":\"code\",\"name\":\"script\",\"content\":\"print('hi')\"}]}\n```";

        let result = parse_extraction_response(response, &fallback).unwrap();
        assert_eq!(result.summary, "Done");
        assert!(matches!(result.artifacts[0].kind, ArtifactKind::Code));
        assert_eq!(result.artifacts[0].content, "print('hi')");
    }

    #[test]
    fn parse_extraction_multiple_artifacts() {
        let fallback = sample_agent_output();
        let response = r#"{"summary":"Generated code and docs","artifacts":[{"kind":"code","name":"main.rs","content":"fn main() {}"},{"kind":"text","name":"docs","content":"Documentation here"}]}"#;

        let result = parse_extraction_response(response, &fallback).unwrap();
        assert_eq!(result.artifacts.len(), 2);
        assert!(matches!(result.artifacts[0].kind, ArtifactKind::Code));
        assert!(matches!(result.artifacts[1].kind, ArtifactKind::Text));
    }

    #[test]
    fn parse_extraction_skips_empty_content() {
        let fallback = sample_agent_output();
        let response = r#"{"summary":"Partial","artifacts":[{"kind":"text","name":"empty","content":""},{"kind":"text","name":"real","content":"data"}]}"#;

        let result = parse_extraction_response(response, &fallback).unwrap();
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].name, "real");
    }

    #[test]
    fn parse_extraction_returns_none_for_invalid_json() {
        let fallback = sample_agent_output();
        assert!(parse_extraction_response("not json at all", &fallback).is_none());
    }

    #[test]
    fn parse_extraction_returns_none_for_missing_summary() {
        let fallback = sample_agent_output();
        let response = r#"{"artifacts":[{"kind":"text","name":"r","content":"x"}]}"#;
        assert!(parse_extraction_response(response, &fallback).is_none());
    }

    #[test]
    fn parse_extraction_returns_none_for_empty_artifacts() {
        let fallback = sample_agent_output();
        let response = r#"{"summary":"Nothing","artifacts":[]}"#;
        assert!(parse_extraction_response(response, &fallback).is_none());
    }

    #[test]
    fn parse_extraction_preserves_fallback_usage_and_duration() {
        let fallback = sample_agent_output();
        let response = r#"{"summary":"Done","artifacts":[{"kind":"text","name":"r","content":"ok"}]}"#;

        let result = parse_extraction_response(response, &fallback).unwrap();
        assert_eq!(result.token_usage.prompt_tokens, 100);
        assert_eq!(result.duration, Duration::from_millis(500));
    }
}
