use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorOutputsPullResult, OrchestratorRunDiagnoseResult, OrchestratorStatsGetResult,
    OrchestratorSubagentTurnResult, OrchestratorThreadGetParams, OrchestratorThreadGetResult,
    OrchestratorThreadMessage, OrchestratorThreadResurrectResult, OrchestratorTurnResult,
    ThreadEvent, ThreadSummary,
};
use neuromancer_core::tool::{AgentContext, ToolBroker, ToolCall, ToolResult, ToolSpec};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_core::xdg::{XdgLayout, resolve_path};
use neuromancer_skills::SkillRegistry;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::orchestrator::actions::dispatch::dispatch_tool;
use crate::orchestrator::bootstrap::{build_system0_agent_config, map_xdg_err};
use crate::orchestrator::error::System0Error;
use crate::orchestrator::llm_clients::{build_llm_client, resolve_tool_call_retry_limit};
use crate::orchestrator::prompt::{load_system_prompt_file, render_system0_prompt};
use crate::orchestrator::security::execution_guard::{ExecutionGuard, PlaceholderExecutionGuard};
use crate::orchestrator::skills::SkillToolBroker;
use crate::orchestrator::state::{SYSTEM0_AGENT_ID, SubAgentThreadState, System0ToolBroker};
use crate::orchestrator::tools::effective_system0_tool_allowlist;
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, normalize_error_message, reconstruct_subagent_conversation,
    thread_events_to_thread_messages,
};
use crate::orchestrator::tracing::event_query::{
    event_error_text, event_matches_query, event_tool_id,
};
use crate::orchestrator::tracing::thread_journal::{
    ThreadJournal, make_event, now_rfc3339, subagent_report_task_id, subagent_report_type,
};

const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_THREAD_PAGE_LIMIT: usize = 100;
const DEFAULT_EVENTS_QUERY_LIMIT: usize = 200;

pub struct System0Runtime {
    turn_tx: mpsc::Sender<TurnRequest>,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
    _turn_worker: tokio::task::JoinHandle<()>,
    _report_worker: tokio::task::JoinHandle<()>,
}

struct TurnRequest {
    message: String,
    trigger_type: TriggerType,
    response_tx: oneshot::Sender<Result<OrchestratorTurnResult, System0Error>>,
}

struct System0TurnWorker {
    agent_runtime: Arc<AgentRuntime>,
    session_store: InMemorySessionStore,
    session_id: AgentSessionId,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
}

impl System0TurnWorker {
    async fn process_turn(
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

/// The System0 orchestrator runtime.
///
/// - `neuromancerd` (daemon): background service providing the RPC surface.
/// - `System0`: primary mediator between the user and sub-agents.
///   Receives admin commands, delegates to sub-agents, facilitates
///   self-improvement, and surfaces errors from sub-agent runs.
impl System0Runtime {
    pub async fn new(
        config: &NeuromancerConfig,
        config_path: &Path,
    ) -> Result<Self, System0Error> {
        let layout = XdgLayout::from_env().map_err(map_xdg_err)?;
        let local_root = layout.runtime_root();
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let thread_journal = ThreadJournal::new(layout.runtime_root().join("threads"))?;
        let (skill_registry, execution_guard) =
            Self::resolve_environment(&layout, config).await?;
        let known_skill_ids = skill_registry.list_names();

        let (report_tx, report_worker) =
            Self::spawn_report_worker(thread_journal.clone());

        let allowlisted_system0_tools =
            effective_system0_tool_allowlist(&config.orchestrator.capabilities.skills);
        let subagents = Self::build_subagents(
            config,
            &layout,
            &config_dir,
            &skill_registry,
            &local_root,
            &execution_guard,
            &report_tx,
        )?;
        let config_snapshot = serde_json::to_value(config)
            .map_err(|err| System0Error::Config(err.to_string()))?;

        let session_store = InMemorySessionStore::new();
        let system0_broker = System0ToolBroker::new(
            subagents,
            config_snapshot,
            &allowlisted_system0_tools,
            session_store.clone(),
            thread_journal.clone(),
            config.orchestrator.self_improvement.clone(),
            &known_skill_ids,
            execution_guard,
        );
        let runtime_broker = system0_broker.clone();

        let system0_agent_runtime = Self::build_system0_agent(
            config,
            &layout,
            &config_dir,
            &allowlisted_system0_tools,
            &system0_broker,
            report_tx,
        )?;

        let (turn_tx, turn_worker) = Self::spawn_turn_worker(
            system0_agent_runtime,
            session_store,
            system0_broker,
            thread_journal.clone(),
        );

        Ok(Self {
            turn_tx,
            system0_broker: runtime_broker,
            thread_journal,
            _turn_worker: turn_worker,
            _report_worker: report_worker,
        })
    }

    async fn resolve_environment(
        layout: &XdgLayout,
        config: &NeuromancerConfig,
    ) -> Result<(SkillRegistry, Arc<dyn ExecutionGuard>), System0Error> {
        let mut skill_registry = SkillRegistry::new(vec![layout.skills_dir()]);
        skill_registry
            .scan()
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?;

        if config.orchestrator.self_improvement.enabled
            && !config
                .agents
                .contains_key(&config.orchestrator.self_improvement.audit_agent_id)
        {
            return Err(System0Error::Config(format!(
                "self-improvement requires audit agent '{}' in [agents]",
                config.orchestrator.self_improvement.audit_agent_id
            )));
        }

        let execution_guard: Arc<dyn ExecutionGuard> = Arc::new(PlaceholderExecutionGuard);
        Ok((skill_registry, execution_guard))
    }

    fn spawn_report_worker(
        thread_journal: ThreadJournal,
    ) -> (
        mpsc::Sender<neuromancer_core::agent::SubAgentReport>,
        tokio::task::JoinHandle<()>,
    ) {
        let (report_tx, mut report_rx) = mpsc::channel(256);
        let worker = tokio::spawn(async move {
            while let Some(report) = report_rx.recv().await {
                let event = make_event(
                    SYSTEM0_AGENT_ID,
                    "system",
                    "subagent_report",
                    None,
                    Some(subagent_report_task_id(&report)),
                    serde_json::json!({
                        "report_type": subagent_report_type(&report),
                        "report": report,
                    }),
                );
                if let Err(err) = thread_journal.append_event(event).await {
                    tracing::error!(error = ?err, "thread_journal_report_write_failed");
                }
            }
        });
        (report_tx, worker)
    }

    fn build_subagents(
        config: &NeuromancerConfig,
        layout: &XdgLayout,
        config_dir: &Path,
        skill_registry: &SkillRegistry,
        local_root: &Path,
        execution_guard: &Arc<dyn ExecutionGuard>,
        report_tx: &mpsc::Sender<neuromancer_core::agent::SubAgentReport>,
    ) -> Result<std::collections::HashMap<String, Arc<AgentRuntime>>, System0Error> {
        let mut subagents = std::collections::HashMap::new();
        for (agent_id, agent_toml) in &config.agents {
            let prompt_path = resolve_path(
                agent_toml.system_prompt_path.as_deref(),
                layout.default_agent_system_prompt_path(agent_id),
                config_dir,
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
                skill_registry,
                local_root.to_path_buf(),
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
        Ok(subagents)
    }

    fn build_system0_agent(
        config: &NeuromancerConfig,
        layout: &XdgLayout,
        config_dir: &Path,
        allowlisted_tools: &[String],
        system0_broker: &System0ToolBroker,
        report_tx: mpsc::Sender<neuromancer_core::agent::SubAgentReport>,
    ) -> Result<Arc<AgentRuntime>, System0Error> {
        let system0_prompt_path = resolve_path(
            config.orchestrator.system_prompt_path.as_deref(),
            layout.default_orchestrator_system_prompt_path(),
            config_dir,
            layout.home_dir(),
        )
        .map_err(map_xdg_err)?;
        let system0_prompt_template = load_system_prompt_file(&system0_prompt_path)?;
        let system0_prompt = render_system0_prompt(
            &system0_prompt_template,
            config.agents.keys().cloned().collect(),
            allowlisted_tools.to_vec(),
        );

        let system0_agent_config =
            build_system0_agent_config(config, allowlisted_tools.to_vec(), system0_prompt);
        let system0_llm = build_llm_client(config, &system0_agent_config)?;
        let system0_tool_call_retry_limit =
            resolve_tool_call_retry_limit(config, &system0_agent_config);
        Ok(Arc::new(AgentRuntime::new(
            system0_agent_config,
            system0_llm,
            Arc::new(system0_broker.clone()),
            report_tx,
            system0_tool_call_retry_limit,
        )))
    }

    fn spawn_turn_worker(
        system0_agent_runtime: Arc<AgentRuntime>,
        session_store: InMemorySessionStore,
        system0_broker: System0ToolBroker,
        thread_journal: ThreadJournal,
    ) -> (
        mpsc::Sender<TurnRequest>,
        tokio::task::JoinHandle<()>,
    ) {
        let session_id = uuid::Uuid::new_v4();
        let core = Arc::new(AsyncMutex::new(System0TurnWorker {
            agent_runtime: system0_agent_runtime,
            session_store: session_store.clone(),
            session_id,
            system0_broker,
            thread_journal,
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
                            "turn_worker_panic"
                        );
                        Err(System0Error::Internal(format!(
                            "turn worker panicked: {join_err}"
                        )))
                    }
                };
                let _ = response_tx.send(result);
            }
        });
        (turn_tx, turn_worker)
    }

    pub async fn turn(
        &self,
        message: String,
    ) -> Result<OrchestratorTurnResult, System0Error> {
        if message.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        // Enqueue the turn and await the result via a oneshot channel.
        let (response_tx, response_rx) = oneshot::channel();
        self.turn_tx
            .send(TurnRequest {
                message,
                trigger_type: TriggerType::Admin,
                response_tx,
            })
            .await
            .map_err(|err| System0Error::Unavailable(err.to_string()))?;

        let received = tokio::time::timeout(TURN_TIMEOUT, response_rx)
            .await
            .map_err(|_| {
                System0Error::Timeout(
                    humantime::format_duration(TURN_TIMEOUT).to_string(),
                )
            })?;

        received
            .map_err(|_| System0Error::Unavailable("turn worker stopped".to_string()))?
    }

    pub async fn runs_list(
        &self,
    ) -> Result<Vec<DelegatedRun>, System0Error> {
        Ok(self.system0_broker.list_runs().await)
    }

    pub async fn outputs_pull(
        &self,
        limit: Option<usize>,
    ) -> Result<OrchestratorOutputsPullResult, System0Error> {
        let limit = limit.unwrap_or(100).clamp(1, 1_000);
        let (outputs, remaining) = self.system0_broker.pull_outputs(limit).await;
        Ok(OrchestratorOutputsPullResult { outputs, remaining })
    }

    pub async fn run_get(
        &self,
        run_id: String,
    ) -> Result<DelegatedRun, System0Error> {
        if run_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        self.system0_broker
            .get_run(&run_id)
            .await
            .ok_or_else(|| System0Error::ResourceNotFound(format!("run '{run_id}'")))
    }

    pub async fn context_get(
        &self,
    ) -> Result<Vec<OrchestratorThreadMessage>, System0Error> {
        let events = self.thread_journal.read_thread_events(SYSTEM0_AGENT_ID)?;
        Ok(thread_events_to_thread_messages(&events))
    }

    pub async fn threads_list(
        &self,
    ) -> Result<Vec<ThreadSummary>, System0Error> {
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

    pub async fn thread_get(
        &self,
        params: OrchestratorThreadGetParams,
    ) -> Result<OrchestratorThreadGetResult, System0Error> {
        if params.thread_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
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

    pub async fn thread_resurrect(
        &self,
        thread_id: String,
    ) -> Result<OrchestratorThreadResurrectResult, System0Error> {
        if thread_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
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
            return Err(System0Error::ResourceNotFound(format!(
                "thread '{thread_id}'"
            )));
        };
        let Some(agent_id) = summary.agent_id.clone() else {
            return Err(System0Error::InvalidRequest(format!(
                "thread '{thread_id}' is not a sub-agent thread"
            )));
        };

        let _runtime = self
            .system0_broker
            .runtime_for_agent(&agent_id)
            .await
            .ok_or_else(|| {
                System0Error::ResourceNotFound(format!(
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
            .append_event(make_event(
                thread_id.clone(),
                "subagent",
                "thread_resurrected",
                Some(agent_id),
                resurrected.latest_run_id.clone(),
                serde_json::json!({"resurrected": true}),
            ))
            .await?;

        Ok(OrchestratorThreadResurrectResult {
            thread: resurrected,
        })
    }

    pub async fn subagent_turn(
        &self,
        thread_id: String,
        message: String,
    ) -> Result<OrchestratorSubagentTurnResult, System0Error> {
        if thread_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        if message.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        let state = self
            .system0_broker
            .get_thread_state(&thread_id)
            .await
            .ok_or_else(|| {
                System0Error::ResourceNotFound(format!("thread '{thread_id}'"))
            })?;

        let runtime = self
            .system0_broker
            .runtime_for_agent(&state.agent_id)
            .await
            .ok_or_else(|| {
                System0Error::ResourceNotFound(format!("agent '{}'", state.agent_id))
            })?;
        let session_store = self.system0_broker.session_store().await;
        let run_result = runtime
            .execute_turn(
                &session_store,
                state.session_id,
                TriggerSource::Internal,
                message.clone(),
            )
            .await;
        let run_id = run_result
            .as_ref()
            .map(|run| run.task_id.to_string())
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
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
            System0Error::Internal(format!(
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
                .append_event(make_event(
                    thread_id.clone(),
                    "subagent",
                    "error",
                    Some(state.agent_id.clone()),
                    Some(run_id.clone()),
                    serde_json::json!({
                        "error": err,
                        "tool_id": "orchestrator.subagent.turn",
                    }),
                ))
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
            .append_event(make_event(
                thread_id.clone(),
                "subagent",
                "run_state_changed",
                Some(state.agent_id),
                Some(run_id),
                serde_json::json!({
                    "state": run_state,
                    "summary": delegated_run.summary,
                    "error": delegated_run.error,
                }),
            ))
            .await?;

        Ok(OrchestratorSubagentTurnResult {
            thread_id,
            run: delegated_run,
            response,
            tool_invocations: Vec::new(),
        })
    }

    pub async fn events_query(
        &self,
        params: OrchestratorEventsQueryParams,
    ) -> Result<OrchestratorEventsQueryResult, System0Error> {
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

    pub async fn run_diagnose(
        &self,
        run_id: String,
    ) -> Result<OrchestratorRunDiagnoseResult, System0Error> {
        if run_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        let run =
            self.system0_broker.get_run(&run_id).await.ok_or_else(|| {
                System0Error::ResourceNotFound(format!("run '{run_id}'"))
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
            let summaries = self.threads_list().await?;
            summaries
                .into_iter()
                .find(|summary| summary.thread_id == thread_id)
        } else {
            None
        };

        let inferred_failure_cause = events.iter().rev().find_map(event_error_text);

        Ok(OrchestratorRunDiagnoseResult {
            run,
            thread,
            events,
            inferred_failure_cause,
        })
    }

    pub async fn stats_get(
        &self,
    ) -> Result<OrchestratorStatsGetResult, System0Error> {
        let events = self.thread_journal.read_all_thread_events()?;
        let runs = self.system0_broker.list_runs().await;
        let threads_total = self.threads_list().await?.len();

        let mut event_counts = BTreeMap::<String, usize>::new();
        let mut tool_counts = BTreeMap::<String, usize>::new();
        let mut agent_counts = BTreeMap::<String, usize>::new();
        for event in &events {
            *event_counts.entry(event.event_type.clone()).or_default() += 1;
            if let Some(tool_id) = event_tool_id(event) {
                *tool_counts.entry(tool_id).or_default() += 1;
            }
            if let Some(agent_id) = event.agent_id.clone() {
                *agent_counts.entry(agent_id).or_default() += 1;
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
            let turn_id = inner.turn.current_turn_id;
            if !inner.improvement.allowlisted_tools.contains(&call.tool_id) {
                let err = NeuromancerError::Tool(ToolError::NotFound {
                    tool_id: call.tool_id.clone(),
                });
                inner.runs.record_invocation_err(turn_id, &call, &err);
                return Err(err);
            }
        }

        dispatch_tool(self, call).await
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
