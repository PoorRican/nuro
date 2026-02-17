use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorReportRecord, OrchestratorReportsQueryParams, OrchestratorReportsQueryResult,
    OrchestratorRunDiagnoseResult, OrchestratorStatsGetResult, OrchestratorSubagentTurnResult,
    OrchestratorThreadGetParams, OrchestratorThreadGetResult, OrchestratorThreadMessage,
    OrchestratorThreadResurrectResult, OrchestratorTurnResult, ThreadEvent, ThreadSummary,
};
use neuromancer_core::tool::{AgentContext, ToolBroker, ToolCall, ToolResult, ToolSpec};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_core::xdg::{XdgLayout, resolve_path};
use neuromancer_skills::SkillRegistry;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::orchestrator::actions::dispatch::dispatch_tool;
use crate::orchestrator::bootstrap::{build_orchestrator_config, map_xdg_err};
use crate::orchestrator::error::OrchestratorRuntimeError;
use crate::orchestrator::llm_clients::{build_llm_client, resolve_tool_call_retry_limit};
use crate::orchestrator::prompt::{load_system_prompt_file, render_orchestrator_prompt};
use crate::orchestrator::security::execution_guard::{ExecutionGuard, PlaceholderExecutionGuard};
use crate::orchestrator::skills::SkillToolBroker;
use crate::orchestrator::state::{
    ActiveRunContext, SYSTEM0_AGENT_ID, SubAgentThreadState, System0ToolBroker,
};
use crate::orchestrator::tools::effective_system0_tool_allowlist;
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, normalize_error_message, reconstruct_subagent_conversation,
    thread_events_to_thread_messages,
};
use crate::orchestrator::tracing::event_query::{
    event_error_text, event_matches_query, event_tool_id,
};
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, now_rfc3339};

const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_THREAD_PAGE_LIMIT: usize = 100;
const DEFAULT_EVENTS_QUERY_LIMIT: usize = 200;
const DEFAULT_REPORTS_QUERY_LIMIT: usize = 200;

pub struct OrchestratorRuntime {
    turn_tx: mpsc::Sender<TurnRequest>,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
    _turn_worker: tokio::task::JoinHandle<()>,
    _report_worker: tokio::task::JoinHandle<()>,
}

struct TurnRequest {
    message: String,
    trigger_type: TriggerType,
    response_tx: oneshot::Sender<Result<OrchestratorTurnResult, OrchestratorRuntimeError>>,
}

struct RuntimeCore {
    orchestrator_runtime: Arc<AgentRuntime>,
    session_store: InMemorySessionStore,
    session_id: AgentSessionId,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
}

impl RuntimeCore {
    async fn process_turn(
        &mut self,
        message: String,
        trigger_type: TriggerType,
    ) -> Result<OrchestratorTurnResult, OrchestratorRuntimeError> {
        let turn_id = uuid::Uuid::new_v4();
        let turn_started_at = Instant::now();
        tracing::info!(
            turn_id = %turn_id,
            trigger_type = ?trigger_type,
            message_chars = message.len(),
            "orchestrator_turn_started"
        );
        self.system0_broker
            .set_turn_context(turn_id, trigger_type)
            .await;

        let _ = self
            .thread_journal
            .append_event(ThreadEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: SYSTEM0_AGENT_ID.to_string(),
                thread_kind: "system".to_string(),
                seq: 0,
                ts: now_rfc3339(),
                event_type: "message_user".to_string(),
                agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                run_id: Some(turn_id.to_string()),
                payload: serde_json::json!({ "role": "user", "content": message.clone() }),
                redaction_applied: false,
                turn_id: Some(turn_id.to_string()),
                parent_event_id: None,
                call_id: None,
                attempt: None,
                duration_ms: None,
                meta: None,
            })
            .await;

        let output = match self
            .orchestrator_runtime
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
                let _ = self
                    .thread_journal
                    .append_event(ThreadEvent {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        thread_id: SYSTEM0_AGENT_ID.to_string(),
                        thread_kind: "system".to_string(),
                        seq: 0,
                        ts: now_rfc3339(),
                        event_type: "error".to_string(),
                        agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                        run_id: Some(turn_id.to_string()),
                        payload: serde_json::json!({
                            "error": normalized,
                            "message": "orchestrator turn failed",
                        }),
                        redaction_applied: false,
                        turn_id: Some(turn_id.to_string()),
                        parent_event_id: None,
                        call_id: None,
                        attempt: None,
                        duration_ms: None,
                        meta: None,
                    })
                    .await;
                tracing::error!(
                    turn_id = %turn_id,
                    error = ?err,
                    duration_ms = turn_started_at.elapsed().as_millis(),
                    "orchestrator_turn_failed"
                );
                return Err(OrchestratorRuntimeError::Internal(err.to_string()));
            }
        };

        let response =
            extract_response_text(&output.output).unwrap_or_else(|| output.output.summary.clone());
        let delegated_runs = self.system0_broker.take_runs(turn_id).await;
        let tool_invocations = self.system0_broker.take_tool_invocations(turn_id).await;

        for invocation in &tool_invocations {
            let _ = self
                .thread_journal
                .append_event(ThreadEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    thread_id: SYSTEM0_AGENT_ID.to_string(),
                    thread_kind: "system".to_string(),
                    seq: 0,
                    ts: now_rfc3339(),
                    event_type: "tool_call".to_string(),
                    agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                    run_id: Some(turn_id.to_string()),
                    payload: serde_json::json!({
                        "call_id": invocation.call_id,
                        "tool_id": invocation.tool_id,
                        "arguments": invocation.arguments,
                    }),
                    redaction_applied: false,
                    turn_id: Some(turn_id.to_string()),
                    parent_event_id: None,
                    call_id: Some(invocation.call_id.clone()),
                    attempt: None,
                    duration_ms: None,
                    meta: None,
                })
                .await;
            let _ = self
                .thread_journal
                .append_event(ThreadEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    thread_id: SYSTEM0_AGENT_ID.to_string(),
                    thread_kind: "system".to_string(),
                    seq: 0,
                    ts: now_rfc3339(),
                    event_type: "tool_result".to_string(),
                    agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                    run_id: Some(turn_id.to_string()),
                    payload: serde_json::json!({
                        "call_id": invocation.call_id,
                        "tool_id": invocation.tool_id,
                        "status": invocation.status,
                        "output": invocation.output,
                    }),
                    redaction_applied: false,
                    turn_id: Some(turn_id.to_string()),
                    parent_event_id: None,
                    call_id: Some(invocation.call_id.clone()),
                    attempt: None,
                    duration_ms: None,
                    meta: None,
                })
                .await;
        }

        let _ = self
            .thread_journal
            .append_event(ThreadEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: SYSTEM0_AGENT_ID.to_string(),
                thread_kind: "system".to_string(),
                seq: 0,
                ts: now_rfc3339(),
                event_type: "message_assistant".to_string(),
                agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                run_id: Some(turn_id.to_string()),
                payload: serde_json::json!({ "role": "assistant", "content": response.clone() }),
                redaction_applied: false,
                turn_id: Some(turn_id.to_string()),
                parent_event_id: None,
                call_id: None,
                attempt: None,
                duration_ms: Some(turn_started_at.elapsed().as_millis() as u64),
                meta: None,
            })
            .await;
        tracing::info!(
            turn_id = %turn_id,
            delegated_runs = delegated_runs.len(),
            tool_invocations = tool_invocations.len(),
            duration_ms = turn_started_at.elapsed().as_millis(),
            "orchestrator_turn_finished"
        );

        Ok(OrchestratorTurnResult {
            turn_id: turn_id.to_string(),
            response,
            delegated_runs,
            tool_invocations,
        })
    }
}

impl OrchestratorRuntime {
    pub async fn new(
        config: &NeuromancerConfig,
        config_path: &Path,
    ) -> Result<Self, OrchestratorRuntimeError> {
        let layout = XdgLayout::from_env().map_err(map_xdg_err)?;
        let skills_dir = layout.skills_dir();
        let local_root = layout.runtime_root();
        let threads_root = layout.runtime_root().join("threads");
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let thread_journal = ThreadJournal::new(threads_root)?;

        let mut skill_registry = SkillRegistry::new(vec![skills_dir.clone()]);
        skill_registry
            .scan()
            .await
            .map_err(|err| OrchestratorRuntimeError::Config(err.to_string()))?;
        let known_skill_ids = skill_registry.list_names();
        let execution_guard: Arc<dyn ExecutionGuard> = Arc::new(PlaceholderExecutionGuard);

        if config.orchestrator.self_improvement.enabled
            && !config
                .agents
                .contains_key(&config.orchestrator.self_improvement.audit_agent_id)
        {
            return Err(OrchestratorRuntimeError::Config(format!(
                "self-improvement requires audit agent '{}' in [agents]",
                config.orchestrator.self_improvement.audit_agent_id
            )));
        }

        let (report_tx, mut report_rx) = mpsc::channel(256);
        let session_store = InMemorySessionStore::new();
        let session_id = uuid::Uuid::new_v4();

        let allowlisted_system0_tools =
            effective_system0_tool_allowlist(&config.orchestrator.capabilities.skills);
        let mut subagents = std::collections::HashMap::<String, Arc<AgentRuntime>>::new();
        for (agent_id, agent_toml) in &config.agents {
            let prompt_path = resolve_path(
                agent_toml.system_prompt_path.as_deref(),
                layout.default_agent_system_prompt_path(agent_id),
                &config_dir,
                layout.home_dir(),
            )
            .map_err(map_xdg_err)?;
            let system_prompt = load_system_prompt_file(&prompt_path)?;
            let agent_config = agent_toml.to_agent_config(agent_id, system_prompt);

            let llm_client = build_llm_client(config, &agent_config)?;
            let tool_call_retry_limit = resolve_tool_call_retry_limit(config, &agent_config);
            let broker: Arc<dyn ToolBroker> = Arc::new(SkillToolBroker::new(
                agent_id,
                &agent_config.capabilities.skills,
                &skill_registry,
                local_root.clone(),
                execution_guard.clone(),
            )?);

            let runtime = Arc::new(AgentRuntime::new(
                agent_config,
                llm_client,
                broker,
                report_tx.clone(),
                tool_call_retry_limit,
            ));
            subagents.insert(agent_id.clone(), runtime);
        }

        let config_snapshot = serde_json::to_value(config)
            .map_err(|err| OrchestratorRuntimeError::Config(err.to_string()))?;

        let system0_broker = System0ToolBroker::new(
            subagents,
            config_snapshot,
            &allowlisted_system0_tools,
            session_store.clone(),
            thread_journal.clone(),
            config.orchestrator.self_improvement.clone(),
            &known_skill_ids,
            execution_guard.clone(),
        );
        let runtime_broker = system0_broker.clone();
        let report_broker = runtime_broker.clone();
        let report_worker = tokio::spawn(async move {
            while let Some(report) = report_rx.recv().await {
                if let Err(err) = report_broker.ingest_subagent_report(report).await {
                    tracing::error!(error = ?err, "subagent_report_ingest_failed");
                }
            }
        });

        let orchestrator_prompt_path = resolve_path(
            config.orchestrator.system_prompt_path.as_deref(),
            layout.default_orchestrator_system_prompt_path(),
            &config_dir,
            layout.home_dir(),
        )
        .map_err(map_xdg_err)?;
        let orchestrator_prompt_template = load_system_prompt_file(&orchestrator_prompt_path)?;
        let orchestrator_prompt = render_orchestrator_prompt(
            &orchestrator_prompt_template,
            config.agents.keys().cloned().collect(),
            allowlisted_system0_tools.clone(),
        );

        let orchestrator_config =
            build_orchestrator_config(config, allowlisted_system0_tools, orchestrator_prompt);
        let orchestrator_llm = build_llm_client(config, &orchestrator_config)?;
        let orchestrator_tool_call_retry_limit =
            resolve_tool_call_retry_limit(config, &orchestrator_config);
        let orchestrator_runtime = Arc::new(AgentRuntime::new(
            orchestrator_config,
            orchestrator_llm,
            Arc::new(system0_broker.clone()),
            report_tx,
            orchestrator_tool_call_retry_limit,
        ));

        let core = Arc::new(AsyncMutex::new(RuntimeCore {
            orchestrator_runtime,
            session_store: session_store.clone(),
            session_id,
            system0_broker,
            thread_journal: thread_journal.clone(),
        }));

        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnRequest>(128);
        let worker_core = core.clone();
        let turn_worker = tokio::spawn(async move {
            while let Some(request) = turn_rx.recv().await {
                let TurnRequest {
                    message,
                    trigger_type,
                    response_tx,
                } = request;
                let started_at = Instant::now();

                let result = match tokio::spawn({
                    let worker_core = worker_core.clone();
                    async move {
                        let mut core = worker_core.lock().await;
                        core.process_turn(message, trigger_type).await
                    }
                })
                .await
                {
                    Ok(result) => result,
                    Err(join_err) => {
                        tracing::error!(
                            error = ?join_err,
                            duration_ms = started_at.elapsed().as_millis(),
                            "orchestrator_turn_worker_panic"
                        );
                        Err(OrchestratorRuntimeError::Internal(format!(
                            "turn worker panicked: {join_err}"
                        )))
                    }
                };
                let _ = response_tx.send(result);
            }
        });

        Ok(Self {
            turn_tx,
            system0_broker: runtime_broker,
            thread_journal,
            _turn_worker: turn_worker,
            _report_worker: report_worker,
        })
    }

    pub async fn orchestrator_turn(
        &self,
        message: String,
    ) -> Result<OrchestratorTurnResult, OrchestratorRuntimeError> {
        if message.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        let (response_tx, response_rx) = oneshot::channel();
        self.turn_tx
            .send(TurnRequest {
                message,
                trigger_type: TriggerType::Admin,
                response_tx,
            })
            .await
            .map_err(|err| OrchestratorRuntimeError::Unavailable(err.to_string()))?;

        let received = tokio::time::timeout(TURN_TIMEOUT, response_rx)
            .await
            .map_err(|_| {
                OrchestratorRuntimeError::Timeout(
                    humantime::format_duration(TURN_TIMEOUT).to_string(),
                )
            })?;

        received
            .map_err(|_| OrchestratorRuntimeError::Unavailable("turn worker stopped".to_string()))?
    }

    pub async fn orchestrator_runs_list(
        &self,
    ) -> Result<Vec<DelegatedRun>, OrchestratorRuntimeError> {
        Ok(self.system0_broker.list_runs().await)
    }

    pub async fn orchestrator_run_get(
        &self,
        run_id: String,
    ) -> Result<DelegatedRun, OrchestratorRuntimeError> {
        if run_id.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        self.system0_broker
            .get_run(&run_id)
            .await
            .ok_or_else(|| OrchestratorRuntimeError::ResourceNotFound(format!("run '{run_id}'")))
    }

    pub async fn orchestrator_context_get(
        &self,
    ) -> Result<Vec<OrchestratorThreadMessage>, OrchestratorRuntimeError> {
        let events = self.thread_journal.read_thread_events(SYSTEM0_AGENT_ID)?;
        Ok(thread_events_to_thread_messages(&events))
    }

    pub async fn orchestrator_threads_list(
        &self,
    ) -> Result<Vec<ThreadSummary>, OrchestratorRuntimeError> {
        let mut summaries = self.thread_journal.load_latest_thread_summaries()?;
        summaries
            .entry(SYSTEM0_AGENT_ID.to_string())
            .or_insert_with(|| ThreadSummary {
                thread_id: SYSTEM0_AGENT_ID.to_string(),
                kind: "system".to_string(),
                agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                latest_run_id: None,
                state: "active".to_string(),
                updated_at: now_rfc3339(),
                resurrected: false,
                active: true,
            });

        let active_threads = self.system0_broker.list_thread_states().await;
        for state in active_threads {
            summaries.insert(
                state.thread_id.clone(),
                ThreadSummary {
                    thread_id: state.thread_id,
                    kind: "subagent".to_string(),
                    agent_id: Some(state.agent_id),
                    latest_run_id: state.latest_run_id,
                    state: state.state,
                    updated_at: state.updated_at,
                    resurrected: state.resurrected,
                    active: state.active,
                },
            );
        }

        let mut out = summaries.into_values().collect::<Vec<_>>();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    pub async fn orchestrator_thread_get(
        &self,
        params: OrchestratorThreadGetParams,
    ) -> Result<OrchestratorThreadGetResult, OrchestratorRuntimeError> {
        if params.thread_id.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_THREAD_PAGE_LIMIT)
            .clamp(1, 500);

        let events = self.thread_journal.read_thread_events(&params.thread_id)?;
        let total = events.len();
        let page = events
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(OrchestratorThreadGetResult {
            thread_id: params.thread_id,
            events: page,
            total,
            offset,
            limit,
        })
    }

    pub async fn orchestrator_thread_resurrect(
        &self,
        thread_id: String,
    ) -> Result<OrchestratorThreadResurrectResult, OrchestratorRuntimeError> {
        if thread_id.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        if thread_id == SYSTEM0_AGENT_ID {
            return Ok(OrchestratorThreadResurrectResult {
                thread: ThreadSummary {
                    thread_id: SYSTEM0_AGENT_ID.to_string(),
                    kind: "system".to_string(),
                    agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                    latest_run_id: None,
                    state: "active".to_string(),
                    updated_at: now_rfc3339(),
                    resurrected: false,
                    active: true,
                },
            });
        }

        let summaries = self.thread_journal.load_latest_thread_summaries()?;
        let Some(summary) = summaries.get(&thread_id).cloned() else {
            return Err(OrchestratorRuntimeError::ResourceNotFound(format!(
                "thread '{thread_id}'"
            )));
        };
        let Some(agent_id) = summary.agent_id.clone() else {
            return Err(OrchestratorRuntimeError::InvalidRequest(format!(
                "thread '{thread_id}' is not a sub-agent thread"
            )));
        };

        let _runtime = self
            .system0_broker
            .runtime_for_agent(&agent_id)
            .await
            .ok_or_else(|| {
                OrchestratorRuntimeError::ResourceNotFound(format!(
                    "agent '{agent_id}' for thread '{thread_id}'"
                ))
            })?;

        let events = self.thread_journal.read_thread_events(&thread_id)?;
        let session_id = uuid::Uuid::new_v4();
        let reconstructed = reconstruct_subagent_conversation(&events);
        self.system0_broker
            .session_store()
            .await
            .save_conversation(session_id, reconstructed.clone())
            .await;

        let thread_state = SubAgentThreadState {
            thread_id: thread_id.clone(),
            agent_id: agent_id.clone(),
            session_id,
            latest_run_id: summary.latest_run_id.clone(),
            state: "active".to_string(),
            summary: None,
            initial_instruction: None,
            resurrected: true,
            active: true,
            updated_at: now_rfc3339(),
            persisted_message_count: conversation_to_thread_messages(&reconstructed.messages).len(),
        };
        self.system0_broker.upsert_thread_state(thread_state).await;

        let resurrected = ThreadSummary {
            resurrected: true,
            active: true,
            updated_at: now_rfc3339(),
            ..summary
        };
        self.thread_journal
            .append_index_snapshot(&resurrected)
            .await?;
        self.thread_journal
            .append_event(ThreadEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: thread_id.clone(),
                thread_kind: "subagent".to_string(),
                seq: 0,
                ts: now_rfc3339(),
                event_type: "thread_resurrected".to_string(),
                agent_id: Some(agent_id),
                run_id: resurrected.latest_run_id.clone(),
                payload: serde_json::json!({"resurrected": true}),
                redaction_applied: false,
                turn_id: None,
                parent_event_id: None,
                call_id: None,
                attempt: None,
                duration_ms: None,
                meta: None,
            })
            .await?;

        Ok(OrchestratorThreadResurrectResult {
            thread: resurrected,
        })
    }

    pub async fn orchestrator_subagent_turn(
        &self,
        thread_id: String,
        message: String,
    ) -> Result<OrchestratorSubagentTurnResult, OrchestratorRuntimeError> {
        if thread_id.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        if message.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        let state = self
            .system0_broker
            .get_thread_state(&thread_id)
            .await
            .ok_or_else(|| {
                OrchestratorRuntimeError::ResourceNotFound(format!("thread '{thread_id}'"))
            })?;

        let runtime = self
            .system0_broker
            .runtime_for_agent(&state.agent_id)
            .await
            .ok_or_else(|| {
                OrchestratorRuntimeError::ResourceNotFound(format!("agent '{}'", state.agent_id))
            })?;
        let session_store = self.system0_broker.session_store().await;
        let run_uuid = uuid::Uuid::new_v4();
        let run_id = run_uuid.to_string();
        self.system0_broker
            .register_active_run(ActiveRunContext {
                run_id: run_id.clone(),
                agent_id: state.agent_id.clone(),
                thread_id: thread_id.clone(),
                turn_id: None,
                call_id: None,
            })
            .await;
        let run_result = runtime
            .execute_turn_with_task_id(
                &session_store,
                state.session_id,
                TriggerSource::Internal,
                message.clone(),
                run_uuid,
            )
            .await;
        let (run_state, response, error) = match run_result {
            Ok(run) => (
                "completed".to_string(),
                extract_response_text(&run.output).unwrap_or_else(|| run.output.summary.clone()),
                None,
            ),
            Err(err) => {
                let normalized = normalize_error_message(err.to_string());
                ("failed".to_string(), normalized.clone(), Some(normalized))
            }
        };

        let conversation = session_store.get(state.session_id).await.ok_or_else(|| {
            OrchestratorRuntimeError::Internal(format!(
                "missing conversation for thread '{thread_id}'"
            ))
        })?;
        let thread_messages = conversation_to_thread_messages(&conversation.conversation.messages);
        let delta = thread_messages
            .iter()
            .skip(state.persisted_message_count)
            .cloned()
            .collect::<Vec<_>>();

        self.thread_journal
            .append_messages(
                &thread_id,
                "subagent",
                Some(&state.agent_id),
                Some(&run_id),
                &delta,
            )
            .await?;
        if let Some(err) = &error {
            self.thread_journal
                .append_event(ThreadEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    thread_id: thread_id.clone(),
                    thread_kind: "subagent".to_string(),
                    seq: 0,
                    ts: now_rfc3339(),
                    event_type: "error".to_string(),
                    agent_id: Some(state.agent_id.clone()),
                    run_id: Some(run_id.clone()),
                    payload: serde_json::json!({
                        "error": err,
                        "tool_id": "orchestrator.subagent.turn",
                    }),
                    redaction_applied: false,
                    turn_id: None,
                    parent_event_id: None,
                    call_id: None,
                    attempt: None,
                    duration_ms: None,
                    meta: None,
                })
                .await?;
        }

        let delegated_run = DelegatedRun {
            run_id: run_id.clone(),
            agent_id: state.agent_id.clone(),
            state: run_state.clone(),
            summary: Some(response.clone()),
            thread_id: Some(thread_id.clone()),
            initial_instruction: Some(message),
            error: error.clone(),
        };
        self.system0_broker
            .record_subagent_turn_result(&thread_id, delegated_run.clone(), thread_messages.len())
            .await;

        let snapshot = ThreadSummary {
            thread_id: thread_id.clone(),
            kind: "subagent".to_string(),
            agent_id: Some(state.agent_id.clone()),
            latest_run_id: Some(run_id.clone()),
            state: run_state.clone(),
            updated_at: now_rfc3339(),
            resurrected: state.resurrected,
            active: true,
        };
        self.thread_journal.append_index_snapshot(&snapshot).await?;
        self.thread_journal
            .append_event(ThreadEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: thread_id.clone(),
                thread_kind: "subagent".to_string(),
                seq: 0,
                ts: now_rfc3339(),
                event_type: "run_state_changed".to_string(),
                agent_id: Some(state.agent_id),
                run_id: Some(run_id),
                payload: serde_json::json!({
                    "state": run_state,
                    "summary": delegated_run.summary,
                    "error": delegated_run.error,
                }),
                redaction_applied: false,
                turn_id: None,
                parent_event_id: None,
                call_id: None,
                attempt: None,
                duration_ms: None,
                meta: None,
            })
            .await?;

        Ok(OrchestratorSubagentTurnResult {
            thread_id,
            run: delegated_run,
            response,
            tool_invocations: Vec::new(),
        })
    }

    pub async fn orchestrator_events_query(
        &self,
        params: OrchestratorEventsQueryParams,
    ) -> Result<OrchestratorEventsQueryResult, OrchestratorRuntimeError> {
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_EVENTS_QUERY_LIMIT)
            .clamp(1, 1000);

        let mut events = self.thread_journal.read_all_thread_events()?;
        events.retain(|event| event_matches_query(event, &params));

        let total = events.len();
        let events = events
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(OrchestratorEventsQueryResult {
            events,
            total,
            offset,
            limit,
        })
    }

    pub async fn orchestrator_reports_query(
        &self,
        params: OrchestratorReportsQueryParams,
    ) -> Result<OrchestratorReportsQueryResult, OrchestratorRuntimeError> {
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_REPORTS_QUERY_LIMIT)
            .clamp(1, 1000);
        let include_remediation = params.include_remediation.unwrap_or(true);
        let report_type_filter = params
            .report_type
            .as_deref()
            .map(|value| value.to_ascii_lowercase());

        let events = self.thread_journal.read_all_thread_events()?;

        let mut remediation_by_key =
            HashMap::<(String, Option<String>, String), serde_json::Value>::new();
        if include_remediation {
            for event in &events {
                if event.event_type != "remediation_action" {
                    continue;
                }
                let Some(source_report_type) = event
                    .payload
                    .get("source_report_type")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };

                remediation_by_key.insert(
                    (
                        event.thread_id.clone(),
                        event.run_id.clone(),
                        source_report_type.to_ascii_lowercase(),
                    ),
                    event.payload.clone(),
                );
            }
        }

        let mut reports = Vec::<OrchestratorReportRecord>::new();
        for event in events {
            if event.event_type != "subagent_report" {
                continue;
            }

            if let Some(thread_id) = params.thread_id.as_deref()
                && event.thread_id != thread_id
            {
                continue;
            }

            if let Some(run_id) = params.run_id.as_deref()
                && event.run_id.as_deref() != Some(run_id)
            {
                continue;
            }

            let source_agent_id = event
                .payload
                .get("source_agent_id")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            if let Some(agent_id) = params.agent_id.as_deref()
                && event.agent_id.as_deref() != Some(agent_id)
                && source_agent_id.as_deref() != Some(agent_id)
            {
                continue;
            }

            let report_type = event
                .payload
                .get("report_type")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_ascii_lowercase();
            if let Some(filter) = report_type_filter.as_deref()
                && report_type != filter
            {
                continue;
            }

            let source_thread_id = event
                .payload
                .get("source_thread_id")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            let report = event
                .payload
                .get("report")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let remediation_action = if include_remediation {
                remediation_by_key
                    .get(&(
                        event.thread_id.clone(),
                        event.run_id.clone(),
                        report_type.clone(),
                    ))
                    .cloned()
                    .or_else(|| {
                        remediation_by_key
                            .iter()
                            .find_map(|((_, run_id, kind), payload)| {
                                if *run_id == event.run_id && *kind == report_type {
                                    Some(payload.clone())
                                } else {
                                    None
                                }
                            })
                    })
            } else {
                None
            };

            reports.push(OrchestratorReportRecord {
                event_id: event.event_id,
                ts: event.ts,
                thread_id: event.thread_id,
                run_id: event.run_id,
                agent_id: event.agent_id,
                source_thread_id,
                source_agent_id,
                report_type,
                report,
                remediation_action,
            });
        }

        reports.sort_by(|a, b| {
            if a.ts == b.ts {
                a.event_id.cmp(&b.event_id)
            } else {
                a.ts.cmp(&b.ts)
            }
        });

        let total = reports.len();
        let reports = reports
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(OrchestratorReportsQueryResult {
            reports,
            total,
            offset,
            limit,
        })
    }

    pub async fn orchestrator_run_diagnose(
        &self,
        run_id: String,
    ) -> Result<OrchestratorRunDiagnoseResult, OrchestratorRuntimeError> {
        if run_id.trim().is_empty() {
            return Err(OrchestratorRuntimeError::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        let run =
            self.system0_broker.get_run(&run_id).await.ok_or_else(|| {
                OrchestratorRuntimeError::ResourceNotFound(format!("run '{run_id}'"))
            })?;

        let all_events = self.thread_journal.read_all_thread_events()?;
        let mut events = all_events
            .iter()
            .filter(|event| event.run_id.as_deref() == Some(run_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        if events.is_empty()
            && let Some(thread_id) = run.thread_id.as_deref()
        {
            events = self
                .thread_journal
                .read_thread_events(thread_id)?
                .into_iter()
                .filter(|event| event.run_id.as_deref() == Some(run_id.as_str()))
                .collect();
        }

        let thread = if let Some(thread_id) = run.thread_id.as_deref() {
            let summaries = self.orchestrator_threads_list().await?;
            summaries
                .into_iter()
                .find(|summary| summary.thread_id == thread_id)
        } else {
            None
        };

        let snapshot = self.system0_broker.last_report_for_run(&run_id).await;
        let latest_report_type = snapshot
            .as_ref()
            .map(|value| value.report_type.clone())
            .or_else(|| {
                events.iter().rev().find_map(|event| {
                    if event.event_type != "subagent_report" {
                        return None;
                    }
                    event
                        .payload
                        .get("report_type")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                })
            });
        let latest_report = snapshot
            .as_ref()
            .map(|value| value.report.clone())
            .or_else(|| {
                events.iter().rev().find_map(|event| {
                    if event.event_type != "subagent_report" {
                        return None;
                    }
                    event.payload.get("report").cloned()
                })
            });
        let recommended_remediation = snapshot
            .as_ref()
            .and_then(|value| value.recommended_remediation.clone())
            .or_else(|| {
                events.iter().rev().find_map(|event| {
                    if event.event_type != "remediation_action" {
                        return None;
                    }
                    event
                        .payload
                        .get("recommended_remediation")
                        .cloned()
                        .or_else(|| event.payload.get("action_payload").cloned())
                })
            });

        let inferred_failure_cause = latest_report_type
            .as_deref()
            .and_then(|report_type| {
                latest_report
                    .as_ref()
                    .and_then(|report| infer_failure_from_subagent_report(report_type, report))
            })
            .or_else(|| events.iter().rev().find_map(event_error_text));

        Ok(OrchestratorRunDiagnoseResult {
            run,
            thread,
            events,
            inferred_failure_cause,
            latest_report_type,
            latest_report,
            recommended_remediation,
        })
    }

    pub async fn orchestrator_stats_get(
        &self,
    ) -> Result<OrchestratorStatsGetResult, OrchestratorRuntimeError> {
        let events = self.thread_journal.read_all_thread_events()?;
        let runs = self.system0_broker.list_runs().await;
        let threads_total = self.orchestrator_threads_list().await?.len();

        let mut event_counts = BTreeMap::<String, usize>::new();
        let mut tool_counts = BTreeMap::<String, usize>::new();
        let mut agent_counts = BTreeMap::<String, usize>::new();
        let mut subagent_report_counts = BTreeMap::<String, usize>::new();
        let mut remediation_action_counts = BTreeMap::<String, usize>::new();
        for event in &events {
            *event_counts.entry(event.event_type.clone()).or_default() += 1;
            if let Some(tool_id) = event_tool_id(event) {
                *tool_counts.entry(tool_id).or_default() += 1;
            }
            if let Some(agent_id) = event.agent_id.clone() {
                *agent_counts.entry(agent_id).or_default() += 1;
            }
            if event.event_type == "subagent_report"
                && let Some(report_type) = event
                    .payload
                    .get("report_type")
                    .and_then(|value| value.as_str())
            {
                *subagent_report_counts
                    .entry(report_type.to_ascii_lowercase())
                    .or_default() += 1;
            }
            if event.event_type == "remediation_action"
                && let Some(action) = event.payload.get("action").and_then(|value| value.as_str())
            {
                *remediation_action_counts
                    .entry(action.to_ascii_lowercase())
                    .or_default() += 1;
            }
        }

        let failed_runs = runs
            .iter()
            .filter(|run| run.state == "failed" || run.state == "error")
            .count();

        Ok(OrchestratorStatsGetResult {
            threads_total,
            events_total: events.len(),
            runs_total: runs.len(),
            failed_runs,
            event_counts,
            tool_counts,
            agent_counts,
            subagent_report_counts,
            remediation_action_counts,
        })
    }
}

#[async_trait::async_trait]
impl ToolBroker for System0ToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let inner = self.inner.lock().await;
        System0ToolBroker::visible_tool_specs(&inner, &ctx.allowed_tools)
    }

    async fn call_tool(
        &self,
        _ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        {
            let mut inner = self.inner.lock().await;
            let turn_id = inner.current_turn_id;
            if !inner.allowlisted_tools.contains(&call.tool_id) {
                let err = NeuromancerError::Tool(ToolError::NotFound {
                    tool_id: call.tool_id.clone(),
                });
                System0ToolBroker::record_invocation_err(&mut inner, turn_id, &call, &err);
                return Err(err);
            }
        }

        dispatch_tool(self, call).await
    }
}

fn infer_failure_from_subagent_report(
    report_type: &str,
    report: &serde_json::Value,
) -> Option<String> {
    match report_type {
        "stuck" => report
            .get("reason")
            .and_then(|value| value.as_str())
            .map(|value| format!("sub-agent stuck: {value}")),
        "tool_failure" => {
            let tool_id = report.get("tool_id").and_then(|value| value.as_str());
            let error = report.get("error").and_then(|value| value.as_str());
            match (tool_id, error) {
                (Some(tool), Some(error)) => Some(format!("tool failure ({tool}): {error}")),
                (None, Some(error)) => Some(error.to_string()),
                _ => None,
            }
        }
        "policy_denied" => {
            let code = report.get("policy_code").and_then(|value| value.as_str());
            let capability = report
                .get("capability_needed")
                .and_then(|value| value.as_str());
            match (code, capability) {
                (Some(code), Some(capability)) => {
                    Some(format!("policy denied ({code}): {capability}"))
                }
                (_, Some(capability)) => Some(format!("policy denied: {capability}")),
                _ => None,
            }
        }
        "input_required" => report
            .get("question")
            .and_then(|value| value.as_str())
            .map(|value| format!("input required: {value}")),
        "failed" => report
            .get("error")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        _ => None,
    }
}

pub(crate) fn extract_response_text(output: &neuromancer_core::task::TaskOutput) -> Option<String> {
    output
        .artifacts
        .first()
        .map(|artifact| artifact.content.clone())
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;
