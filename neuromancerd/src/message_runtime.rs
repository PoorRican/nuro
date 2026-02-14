use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use neuromancer_agent::conversation::{ChatMessage, ConversationContext, TruncationStrategy};
use neuromancer_agent::llm::{LlmClient, RigLlmClient};
use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::agent::{AgentConfig, AgentHealthConfig, AgentMode, AgentModelConfig};
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorSubagentTurnResult, OrchestratorThreadGetParams,
    OrchestratorThreadGetResult, OrchestratorThreadMessage, OrchestratorThreadResurrectResult,
    OrchestratorToolInvocation, OrchestratorTurnResult, ThreadEvent, ThreadSummary,
};
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult, ToolSource, ToolSpec,
};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_core::xdg::{XdgLayout, resolve_path, validate_markdown_prompt_file};
use neuromancer_skills::{Skill, SkillRegistry};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const SYSTEM0_AGENT_ID: &str = "system0";
const DEFAULT_SKILL_SCRIPT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_THREAD_PAGE_LIMIT: usize = 100;

#[derive(Debug, Clone)]
struct SubAgentThreadState {
    thread_id: String,
    agent_id: String,
    session_id: AgentSessionId,
    latest_run_id: Option<String>,
    state: String,
    summary: Option<String>,
    initial_instruction: Option<String>,
    resurrected: bool,
    active: bool,
    updated_at: String,
    persisted_message_count: usize,
}

#[derive(Clone)]
struct ThreadJournal {
    base_dir: PathBuf,
    index_file: PathBuf,
    lock: Arc<AsyncMutex<()>>,
    seq_cache: Arc<AsyncMutex<HashMap<String, u64>>>,
    secret_values: Arc<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ThreadIndexEntry {
    event_id: String,
    ts: String,
    thread: ThreadSummary,
}

impl ThreadJournal {
    fn new(base_dir: PathBuf) -> Result<Self, MessageRuntimeError> {
        let system_dir = base_dir.join("system0");
        let subagents_dir = base_dir.join("subagents");
        fs::create_dir_all(&system_dir).map_err(|err| {
            MessageRuntimeError::Internal(format!(
                "failed to create thread journal system dir '{}': {err}",
                system_dir.display()
            ))
        })?;
        fs::create_dir_all(&subagents_dir).map_err(|err| {
            MessageRuntimeError::Internal(format!(
                "failed to create thread journal subagent dir '{}': {err}",
                subagents_dir.display()
            ))
        })?;

        let index_file = base_dir.join("index.jsonl");
        if !index_file.exists() {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_file)
                .map_err(|err| {
                    MessageRuntimeError::Internal(format!(
                        "failed to create thread index '{}': {err}",
                        index_file.display()
                    ))
                })?;
        }

        Ok(Self {
            base_dir,
            index_file,
            lock: Arc::new(AsyncMutex::new(())),
            seq_cache: Arc::new(AsyncMutex::new(HashMap::new())),
            secret_values: Arc::new(collect_secret_values()),
        })
    }

    fn system_thread_file(&self) -> PathBuf {
        self.base_dir.join("system0").join("system0.jsonl")
    }

    fn subagent_thread_file(&self, thread_id: &str) -> PathBuf {
        let safe = sanitize_thread_file_component(thread_id);
        self.base_dir
            .join("subagents")
            .join(format!("{safe}.jsonl"))
    }

    fn thread_file_for_id(&self, thread_id: &str) -> PathBuf {
        if thread_id == SYSTEM0_AGENT_ID {
            self.system_thread_file()
        } else {
            self.subagent_thread_file(thread_id)
        }
    }

    fn thread_file_for_event(&self, event: &ThreadEvent) -> PathBuf {
        if event.thread_kind == "system" || event.thread_id == SYSTEM0_AGENT_ID {
            self.system_thread_file()
        } else {
            self.subagent_thread_file(&event.thread_id)
        }
    }

    async fn append_event(&self, mut event: ThreadEvent) -> Result<(), MessageRuntimeError> {
        let _guard = self.lock.lock().await;
        let path = self.thread_file_for_event(&event);
        let current_seq = self.current_seq_for_locked(&event.thread_id, &path)?;
        event.seq = if event.seq == 0 {
            current_seq + 1
        } else {
            event.seq.max(current_seq + 1)
        };

        let (payload, redacted, safe) = sanitize_event_payload(
            &event.event_type,
            event.payload,
            self.secret_values.as_ref(),
        );
        if safe {
            event.payload = payload;
            event.redaction_applied = event.redaction_applied || redacted;
        } else {
            event.payload = serde_json::json!({ "omitted": "sensitive_payload" });
            event.redaction_applied = true;
        }

        append_jsonl_line(&path, &event)?;
        let mut cache = self.seq_cache.lock().await;
        cache.insert(event.thread_id, event.seq);
        Ok(())
    }

    async fn append_index_snapshot(
        &self,
        summary: &ThreadSummary,
    ) -> Result<(), MessageRuntimeError> {
        let _guard = self.lock.lock().await;
        let entry = ThreadIndexEntry {
            event_id: uuid::Uuid::new_v4().to_string(),
            ts: now_rfc3339(),
            thread: summary.clone(),
        };
        append_jsonl_line(&self.index_file, &entry)
    }

    fn load_latest_thread_summaries(
        &self,
    ) -> Result<HashMap<String, ThreadSummary>, MessageRuntimeError> {
        let mut by_thread = HashMap::<String, ThreadSummary>::new();
        if self.index_file.exists() {
            for line in read_jsonl_lines(&self.index_file)? {
                if let Ok(entry) = serde_json::from_str::<ThreadIndexEntry>(&line) {
                    by_thread.insert(entry.thread.thread_id.clone(), entry.thread);
                    continue;
                }
                if let Ok(summary) = serde_json::from_str::<ThreadSummary>(&line) {
                    by_thread.insert(summary.thread_id.clone(), summary);
                }
            }
        }

        let subagents_dir = self.base_dir.join("subagents");
        if subagents_dir.exists() {
            for dir_entry in fs::read_dir(&subagents_dir).map_err(|err| {
                MessageRuntimeError::Internal(format!(
                    "failed to read subagent thread dir '{}': {err}",
                    subagents_dir.display()
                ))
            })? {
                let Ok(dir_entry) = dir_entry else { continue };
                let path = dir_entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                let events = read_thread_events_from_path(&path)?;
                let Some(last) = events.last() else { continue };
                let thread_id = events
                    .first()
                    .map(|event| event.thread_id.clone())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or_default()
                            .to_string()
                    });
                by_thread
                    .entry(thread_id.clone())
                    .or_insert_with(|| ThreadSummary {
                        thread_id,
                        kind: "subagent".to_string(),
                        agent_id: last.agent_id.clone(),
                        latest_run_id: last.run_id.clone(),
                        state: infer_thread_state_from_events(&events),
                        updated_at: last.ts.clone(),
                        resurrected: false,
                        active: false,
                    });
            }
        }

        Ok(by_thread)
    }

    fn read_thread_events(&self, thread_id: &str) -> Result<Vec<ThreadEvent>, MessageRuntimeError> {
        let path = self.thread_file_for_id(thread_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_thread_events_from_path(&path)
    }

    async fn append_messages(
        &self,
        thread_id: &str,
        thread_kind: &str,
        agent_id: Option<&str>,
        run_id: Option<&str>,
        messages: &[OrchestratorThreadMessage],
    ) -> Result<(), MessageRuntimeError> {
        for message in messages {
            let (event_type, payload) = match message {
                OrchestratorThreadMessage::Text { role, content } => {
                    let event_type = if role == "user" {
                        "message_user"
                    } else if role == "assistant" {
                        "message_assistant"
                    } else {
                        "message_system"
                    };
                    (
                        event_type.to_string(),
                        serde_json::json!({
                            "role": role,
                            "content": content,
                        }),
                    )
                }
                OrchestratorThreadMessage::ToolInvocation {
                    call_id,
                    tool_id,
                    arguments,
                    status,
                    output,
                } => (
                    "tool_result".to_string(),
                    serde_json::json!({
                        "call_id": call_id,
                        "tool_id": tool_id,
                        "arguments": arguments,
                        "status": status,
                        "output": output,
                    }),
                ),
            };

            self.append_event(ThreadEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: thread_id.to_string(),
                thread_kind: thread_kind.to_string(),
                seq: 0,
                ts: now_rfc3339(),
                event_type,
                agent_id: agent_id.map(|value| value.to_string()),
                run_id: run_id.map(|value| value.to_string()),
                payload,
                redaction_applied: false,
            })
            .await?;
        }
        Ok(())
    }

    fn current_seq_for_locked(
        &self,
        thread_id: &str,
        path: &Path,
    ) -> Result<u64, MessageRuntimeError> {
        if let Some(seq) = self
            .seq_cache
            .try_lock()
            .ok()
            .and_then(|cache| cache.get(thread_id).copied())
        {
            return Ok(seq);
        }

        if !path.exists() {
            return Ok(0);
        }
        let mut max_seq = 0_u64;
        for line in read_jsonl_lines(path)? {
            let Ok(event) = serde_json::from_str::<ThreadEvent>(&line) else {
                continue;
            };
            max_seq = max_seq.max(event.seq);
        }
        Ok(max_seq)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MessageRuntimeError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("runtime unavailable: {0}")]
    Unavailable(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("execution timed out after {0}")]
    Timeout(String),

    #[error("resource not found: {0}")]
    ResourceNotFound(String),

    #[error("path policy violation: {0}")]
    PathViolation(String),

    #[error("internal runtime error: {0}")]
    Internal(String),
}

impl MessageRuntimeError {
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidRequest(_))
    }

    pub fn is_resource_not_found(&self) -> bool {
        matches!(self, Self::ResourceNotFound(_))
    }
}

pub struct MessageRuntime {
    turn_tx: mpsc::Sender<TurnRequest>,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
    _turn_worker: tokio::task::JoinHandle<()>,
    _report_worker: tokio::task::JoinHandle<()>,
}

struct TurnRequest {
    message: String,
    trigger_type: TriggerType,
    response_tx: oneshot::Sender<Result<OrchestratorTurnResult, MessageRuntimeError>>,
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
    ) -> Result<OrchestratorTurnResult, MessageRuntimeError> {
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
                    })
                    .await;
                tracing::error!(
                    turn_id = %turn_id,
                    error = ?err,
                    duration_ms = turn_started_at.elapsed().as_millis(),
                    "orchestrator_turn_failed"
                );
                return Err(MessageRuntimeError::Internal(err.to_string()));
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

impl MessageRuntime {
    pub async fn new(
        config: &NeuromancerConfig,
        config_path: &Path,
    ) -> Result<Self, MessageRuntimeError> {
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
            .map_err(|err| MessageRuntimeError::Config(err.to_string()))?;

        let (report_tx, mut report_rx) = mpsc::channel(256);
        let report_worker = tokio::spawn(async move { while report_rx.recv().await.is_some() {} });
        let session_store = InMemorySessionStore::new();
        let session_id = uuid::Uuid::new_v4();

        let allowlisted_system0_tools =
            effective_system0_tool_allowlist(&config.orchestrator.capabilities.skills);
        let mut subagents = HashMap::<String, Arc<AgentRuntime>>::new();
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
            .map_err(|err| MessageRuntimeError::Config(err.to_string()))?;

        let system0_broker = System0ToolBroker::new(
            subagents,
            config_snapshot,
            &allowlisted_system0_tools,
            session_store.clone(),
            thread_journal.clone(),
        );
        let runtime_broker = system0_broker.clone();

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
                        Err(MessageRuntimeError::Internal(format!(
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
    ) -> Result<OrchestratorTurnResult, MessageRuntimeError> {
        if message.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
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
            .map_err(|err| MessageRuntimeError::Unavailable(err.to_string()))?;

        let received = tokio::time::timeout(TURN_TIMEOUT, response_rx)
            .await
            .map_err(|_| {
                MessageRuntimeError::Timeout(humantime::format_duration(TURN_TIMEOUT).to_string())
            })?;

        received.map_err(|_| MessageRuntimeError::Unavailable("turn worker stopped".to_string()))?
    }

    pub async fn orchestrator_runs_list(&self) -> Result<Vec<DelegatedRun>, MessageRuntimeError> {
        Ok(self.system0_broker.list_runs().await)
    }

    pub async fn orchestrator_run_get(
        &self,
        run_id: String,
    ) -> Result<DelegatedRun, MessageRuntimeError> {
        if run_id.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        self.system0_broker
            .get_run(&run_id)
            .await
            .ok_or_else(|| MessageRuntimeError::ResourceNotFound(format!("run '{run_id}'")))
    }

    pub async fn orchestrator_context_get(
        &self,
    ) -> Result<Vec<OrchestratorThreadMessage>, MessageRuntimeError> {
        let events = self.thread_journal.read_thread_events(SYSTEM0_AGENT_ID)?;
        Ok(thread_events_to_thread_messages(&events))
    }

    pub async fn orchestrator_threads_list(
        &self,
    ) -> Result<Vec<ThreadSummary>, MessageRuntimeError> {
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
    ) -> Result<OrchestratorThreadGetResult, MessageRuntimeError> {
        if params.thread_id.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
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
    ) -> Result<OrchestratorThreadResurrectResult, MessageRuntimeError> {
        if thread_id.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
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
            return Err(MessageRuntimeError::ResourceNotFound(format!(
                "thread '{thread_id}'"
            )));
        };
        let Some(agent_id) = summary.agent_id.clone() else {
            return Err(MessageRuntimeError::InvalidRequest(format!(
                "thread '{thread_id}' is not a sub-agent thread"
            )));
        };

        let _runtime = self
            .system0_broker
            .runtime_for_agent(&agent_id)
            .await
            .ok_or_else(|| {
                MessageRuntimeError::ResourceNotFound(format!(
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
    ) -> Result<OrchestratorSubagentTurnResult, MessageRuntimeError> {
        if thread_id.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        if message.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        let state = self
            .system0_broker
            .get_thread_state(&thread_id)
            .await
            .ok_or_else(|| {
                MessageRuntimeError::ResourceNotFound(format!("thread '{thread_id}'"))
            })?;

        let runtime = self
            .system0_broker
            .runtime_for_agent(&state.agent_id)
            .await
            .ok_or_else(|| {
                MessageRuntimeError::ResourceNotFound(format!("agent '{}'", state.agent_id))
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
            MessageRuntimeError::Internal(format!("missing conversation for thread '{thread_id}'"))
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
            })
            .await?;

        Ok(OrchestratorSubagentTurnResult {
            thread_id,
            run: delegated_run,
            response,
            tool_invocations: Vec::new(),
        })
    }
}

fn conversation_to_thread_messages(
    messages: &[neuromancer_agent::conversation::ChatMessage],
) -> Vec<OrchestratorThreadMessage> {
    use neuromancer_agent::conversation::{MessageContent, MessageRole};

    let mut result = Vec::new();
    // Collect tool call info so we can merge status from subsequent tool results
    let mut pending_tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();

    for msg in messages {
        match (&msg.role, &msg.content) {
            (MessageRole::System, _) => {
                // Skip system messages from the thread view
            }
            (MessageRole::User, MessageContent::Text(text)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                result.push(OrchestratorThreadMessage::Text {
                    role: "user".to_string(),
                    content: text.clone(),
                });
            }
            (MessageRole::Assistant, MessageContent::Text(text)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                result.push(OrchestratorThreadMessage::Text {
                    role: "assistant".to_string(),
                    content: text.clone(),
                });
            }
            (MessageRole::Assistant, MessageContent::ToolCalls(calls)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                for call in calls {
                    pending_tool_calls.push((
                        call.id.clone(),
                        call.tool_id.clone(),
                        call.arguments.clone(),
                    ));
                }
            }
            (MessageRole::Tool, MessageContent::ToolResult(tool_result)) => {
                // Find matching pending tool call and emit with status
                if let Some(pos) = pending_tool_calls
                    .iter()
                    .position(|(id, _, _)| *id == tool_result.call_id)
                {
                    let (call_id, tool_id, arguments) = pending_tool_calls.remove(pos);
                    let (status, output) = match &tool_result.output {
                        ToolOutput::Success(v) => ("success".to_string(), v.clone()),
                        ToolOutput::Error(e) => {
                            ("error".to_string(), serde_json::Value::String(e.clone()))
                        }
                    };
                    result.push(OrchestratorThreadMessage::ToolInvocation {
                        call_id,
                        tool_id,
                        arguments,
                        status,
                        output,
                    });
                }
            }
            _ => {}
        }
    }
    flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
    result
}

fn flush_pending_tool_calls(
    pending: &mut Vec<(String, String, serde_json::Value)>,
    result: &mut Vec<OrchestratorThreadMessage>,
) {
    for (call_id, tool_id, arguments) in pending.drain(..) {
        result.push(OrchestratorThreadMessage::ToolInvocation {
            call_id,
            tool_id,
            arguments,
            status: "pending".to_string(),
            output: serde_json::Value::Null,
        });
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn sanitize_thread_file_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "thread".to_string()
    } else {
        out
    }
}

fn append_jsonl_line<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), MessageRuntimeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            MessageRuntimeError::Internal(format!(
                "failed to create directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| {
            MessageRuntimeError::Internal(format!(
                "failed to open jsonl file '{}': {err}",
                path.display()
            ))
        })?;
    serde_json::to_writer(&mut file, value).map_err(|err| {
        MessageRuntimeError::Internal(format!("failed to encode jsonl event: {err}"))
    })?;
    file.write_all(b"\n").map_err(|err| {
        MessageRuntimeError::Internal(format!("failed to write jsonl newline: {err}"))
    })?;
    file.flush().map_err(|err| {
        MessageRuntimeError::Internal(format!("failed to flush jsonl file: {err}"))
    })?;
    Ok(())
}

fn read_jsonl_lines(path: &Path) -> Result<Vec<String>, MessageRuntimeError> {
    let file = fs::File::open(path).map_err(|err| {
        MessageRuntimeError::Internal(format!(
            "failed to open jsonl file '{}': {err}",
            path.display()
        ))
    })?;
    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| {
            MessageRuntimeError::Internal(format!(
                "failed to read jsonl file '{}': {err}",
                path.display()
            ))
        })?;
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    Ok(lines)
}

fn read_thread_events_from_path(path: &Path) -> Result<Vec<ThreadEvent>, MessageRuntimeError> {
    let mut events = Vec::<ThreadEvent>::new();
    for line in read_jsonl_lines(path)? {
        match serde_json::from_str::<ThreadEvent>(&line) {
            Ok(event) => events.push(event),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "skipping_malformed_thread_event"
                );
            }
        }
    }
    events.sort_by(|a, b| {
        if a.seq == b.seq {
            a.ts.cmp(&b.ts)
        } else {
            a.seq.cmp(&b.seq)
        }
    });
    Ok(events)
}

fn collect_secret_values() -> Vec<String> {
    let mut values = Vec::new();
    for (key, value) in std::env::vars() {
        if is_sensitive_key(&key) && !value.trim().is_empty() && value.trim().len() >= 6 {
            values.push(value);
        }
    }
    values.sort();
    values.dedup();
    values
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("auth")
        || key.contains("credential")
        || key.contains("bearer")
}

fn sanitize_event_payload(
    event_type: &str,
    payload: serde_json::Value,
    secret_values: &[String],
) -> (serde_json::Value, bool, bool) {
    let mut redacted = false;
    let Ok(sanitized) = sanitize_json_value(payload, secret_values, &mut redacted, 0) else {
        return (
            serde_json::json!({ "omitted": "sensitive_payload" }),
            true,
            false,
        );
    };

    if !tool_payload_shape_allowed(event_type, &sanitized) {
        return (
            serde_json::json!({ "omitted": "sensitive_payload" }),
            true,
            false,
        );
    }

    (sanitized, redacted, true)
}

fn sanitize_json_value(
    value: serde_json::Value,
    secret_values: &[String],
    redacted: &mut bool,
    depth: usize,
) -> Result<serde_json::Value, ()> {
    if depth > 24 {
        return Err(());
    }

    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, inner) in map {
                if is_sensitive_key(&key) {
                    *redacted = true;
                    out.insert(key, serde_json::Value::String("[REDACTED]".to_string()));
                    continue;
                }
                out.insert(
                    key,
                    sanitize_json_value(inner, secret_values, redacted, depth + 1)?,
                );
            }
            Ok(serde_json::Value::Object(out))
        }
        serde_json::Value::Array(values) => {
            let mut out = Vec::with_capacity(values.len());
            for inner in values {
                out.push(sanitize_json_value(
                    inner,
                    secret_values,
                    redacted,
                    depth + 1,
                )?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::String(mut text) => {
            for secret in secret_values {
                if secret.is_empty() {
                    continue;
                }
                if text.contains(secret) {
                    text = text.replace(secret, "[REDACTED]");
                    *redacted = true;
                }
            }
            Ok(serde_json::Value::String(text))
        }
        other => Ok(other),
    }
}

fn tool_payload_shape_allowed(event_type: &str, payload: &serde_json::Value) -> bool {
    if event_type != "tool_call" && event_type != "tool_result" {
        return true;
    }

    let Some(map) = payload.as_object() else {
        return false;
    };
    let allowed: &[&str] = if event_type == "tool_call" {
        &["call_id", "tool_id", "arguments"]
    } else {
        &[
            "call_id",
            "tool_id",
            "arguments",
            "status",
            "output",
            "error",
        ]
    };
    map.keys()
        .all(|key| allowed.iter().any(|allowed_key| key == allowed_key))
}

fn infer_thread_state_from_events(events: &[ThreadEvent]) -> String {
    for event in events.iter().rev() {
        if event.event_type == "error" {
            return "failed".to_string();
        }
        if event.event_type == "run_state_changed" {
            if let Some(state) = event.payload.get("state").and_then(|value| value.as_str()) {
                return state.to_string();
            }
        }
    }
    "completed".to_string()
}

fn normalize_error_message(message: impl Into<String>) -> String {
    let mut text = message.into().replace('\n', " ");
    if text.len() > 512 {
        text.truncate(512);
        text.push_str("...");
    }
    text
}

fn reconstruct_subagent_conversation(events: &[ThreadEvent]) -> ConversationContext {
    let mut conversation = ConversationContext::new(
        u32::MAX,
        TruncationStrategy::SlidingWindow { keep_last: 50 },
    );

    for message in thread_events_to_thread_messages(events) {
        match message {
            OrchestratorThreadMessage::Text { role, content } => {
                if role == "user" {
                    conversation.add_message(ChatMessage::user(content));
                } else if role == "assistant" {
                    conversation.add_message(ChatMessage::assistant_text(content));
                }
            }
            OrchestratorThreadMessage::ToolInvocation {
                call_id,
                tool_id: _,
                arguments: _,
                status,
                output,
            } => {
                let tool_output = if status == "error" {
                    let text = output
                        .get("error")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                        .unwrap_or_else(|| output.to_string());
                    ToolOutput::Error(text)
                } else {
                    ToolOutput::Success(output)
                };
                conversation.add_message(ChatMessage::tool_result(ToolResult {
                    call_id,
                    output: tool_output,
                }));
            }
        }
    }

    conversation
}

fn thread_events_to_thread_messages(events: &[ThreadEvent]) -> Vec<OrchestratorThreadMessage> {
    let mut out = Vec::new();
    let mut pending_tool_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();

    for event in events {
        match event.event_type.as_str() {
            "message_user" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                {
                    out.push(OrchestratorThreadMessage::Text {
                        role: "user".to_string(),
                        content: content.to_string(),
                    });
                }
            }
            "message_assistant" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                {
                    out.push(OrchestratorThreadMessage::Text {
                        role: "assistant".to_string(),
                        content: content.to_string(),
                    });
                }
            }
            "tool_call" => {
                let Some(call_id) = event
                    .payload
                    .get("call_id")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                let tool_id = event
                    .payload
                    .get("tool_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = event
                    .payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                pending_tool_calls.insert(call_id.to_string(), (tool_id, arguments));
            }
            "tool_result" => {
                let Some(call_id) = event
                    .payload
                    .get("call_id")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                let (tool_id, arguments) =
                    pending_tool_calls.remove(call_id).unwrap_or_else(|| {
                        (
                            event
                                .payload
                                .get("tool_id")
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            event
                                .payload
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                    });
                let status = event
                    .payload
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("success")
                    .to_string();
                let output = event
                    .payload
                    .get("output")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                out.push(OrchestratorThreadMessage::ToolInvocation {
                    call_id: call_id.to_string(),
                    tool_id,
                    arguments,
                    status,
                    output,
                });
            }
            "error" => {
                let err = event
                    .payload
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown runtime error");
                out.push(OrchestratorThreadMessage::Text {
                    role: "system".to_string(),
                    content: format!("SYSTEM ERROR: {err}"),
                });
            }
            _ => {}
        }
    }

    for (call_id, (tool_id, arguments)) in pending_tool_calls {
        out.push(OrchestratorThreadMessage::ToolInvocation {
            call_id,
            tool_id,
            arguments,
            status: "pending".to_string(),
            output: serde_json::Value::Null,
        });
    }

    out
}

fn build_orchestrator_config(
    config: &NeuromancerConfig,
    allowlisted_tools: Vec<String>,
    system_prompt: String,
) -> AgentConfig {
    let mut capabilities = config.orchestrator.capabilities.clone();
    capabilities.skills = allowlisted_tools;
    AgentConfig {
        id: SYSTEM0_AGENT_ID.to_string(),
        mode: AgentMode::Inproc,
        image: None,
        models: AgentModelConfig {
            planner: None,
            executor: config.orchestrator.model_slot.clone(),
            verifier: None,
        },
        capabilities,
        health: AgentHealthConfig::default(),
        system_prompt,
        max_iterations: config.orchestrator.max_iterations,
    }
}

#[derive(Clone)]
struct System0ToolBroker {
    inner: Arc<AsyncMutex<System0BrokerInner>>,
}

struct System0BrokerInner {
    subagents: HashMap<String, Arc<AgentRuntime>>,
    config_snapshot: serde_json::Value,
    allowlisted_tools: HashSet<String>,
    session_store: InMemorySessionStore,
    thread_journal: ThreadJournal,
    current_trigger_type: TriggerType,
    current_turn_id: uuid::Uuid,
    runs_by_turn: HashMap<uuid::Uuid, Vec<DelegatedRun>>,
    tool_invocations_by_turn: HashMap<uuid::Uuid, Vec<OrchestratorToolInvocation>>,
    runs_index: HashMap<String, DelegatedRun>,
    runs_order: Vec<String>,
    running_agents: HashMap<String, String>,
    thread_states: HashMap<String, SubAgentThreadState>,
    thread_id_by_agent: HashMap<String, String>,
}

impl System0ToolBroker {
    fn new(
        subagents: HashMap<String, Arc<AgentRuntime>>,
        config_snapshot: serde_json::Value,
        allowlisted_tools: &[String],
        session_store: InMemorySessionStore,
        thread_journal: ThreadJournal,
    ) -> Self {
        let allowlisted_tools = if allowlisted_tools.is_empty() {
            default_system0_tools().into_iter().collect()
        } else {
            allowlisted_tools.iter().cloned().collect()
        };

        Self {
            inner: Arc::new(AsyncMutex::new(System0BrokerInner {
                subagents,
                config_snapshot,
                allowlisted_tools,
                session_store,
                thread_journal,
                current_trigger_type: TriggerType::Admin,
                current_turn_id: uuid::Uuid::nil(),
                runs_by_turn: HashMap::new(),
                tool_invocations_by_turn: HashMap::new(),
                runs_index: HashMap::new(),
                runs_order: Vec::new(),
                running_agents: HashMap::new(),
                thread_states: HashMap::new(),
                thread_id_by_agent: HashMap::new(),
            })),
        }
    }

    async fn set_turn_context(&self, turn_id: uuid::Uuid, trigger_type: TriggerType) {
        let mut inner = self.inner.lock().await;
        inner.current_turn_id = turn_id;
        inner.current_trigger_type = trigger_type;
        inner.runs_by_turn.remove(&turn_id);
        inner.tool_invocations_by_turn.remove(&turn_id);
    }

    async fn take_runs(&self, turn_id: uuid::Uuid) -> Vec<DelegatedRun> {
        let mut inner = self.inner.lock().await;
        inner.runs_by_turn.remove(&turn_id).unwrap_or_default()
    }

    async fn take_tool_invocations(&self, turn_id: uuid::Uuid) -> Vec<OrchestratorToolInvocation> {
        let mut inner = self.inner.lock().await;
        inner
            .tool_invocations_by_turn
            .remove(&turn_id)
            .unwrap_or_default()
    }

    async fn list_runs(&self) -> Vec<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner
            .runs_order
            .iter()
            .filter_map(|run_id| inner.runs_index.get(run_id).cloned())
            .collect()
    }

    async fn get_run(&self, run_id: &str) -> Option<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner.runs_index.get(run_id).cloned()
    }

    async fn session_store(&self) -> InMemorySessionStore {
        let inner = self.inner.lock().await;
        inner.session_store.clone()
    }

    async fn runtime_for_agent(&self, agent_id: &str) -> Option<Arc<AgentRuntime>> {
        let inner = self.inner.lock().await;
        inner.subagents.get(agent_id).cloned()
    }

    async fn list_thread_states(&self) -> Vec<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        let mut states = inner.thread_states.values().cloned().collect::<Vec<_>>();
        states.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        states
    }

    async fn get_thread_state(&self, thread_id: &str) -> Option<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        inner.thread_states.get(thread_id).cloned()
    }

    async fn upsert_thread_state(&self, state: SubAgentThreadState) {
        let mut inner = self.inner.lock().await;
        inner
            .thread_id_by_agent
            .insert(state.agent_id.clone(), state.thread_id.clone());
        inner.thread_states.insert(state.thread_id.clone(), state);
    }

    async fn record_subagent_turn_result(
        &self,
        thread_id: &str,
        run: DelegatedRun,
        persisted_message_count: usize,
    ) {
        let mut inner = self.inner.lock().await;
        inner.runs_index.insert(run.run_id.clone(), run.clone());
        if !inner.runs_order.iter().any(|id| id == &run.run_id) {
            inner.runs_order.push(run.run_id.clone());
        }
        if let Some(state) = inner.thread_states.get_mut(thread_id) {
            state.latest_run_id = Some(run.run_id.clone());
            state.state = run.state.clone();
            state.summary = run.summary.clone();
            if state.initial_instruction.is_none() {
                state.initial_instruction = run.initial_instruction.clone();
            }
            state.active = true;
            state.updated_at = now_rfc3339();
            state.persisted_message_count = persisted_message_count;
        }
    }

    fn build_tool_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                id: "delegate_to_agent".to_string(),
                name: "delegate_to_agent".to_string(),
                description:
                    "Delegate an instruction to a configured sub-agent. args: {agent_id, instruction}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "instruction": {"type": "string"}
                    },
                    "required": ["agent_id", "instruction"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "list_agents".to_string(),
                name: "list_agents".to_string(),
                description: "List configured sub-agents.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "read_config".to_string(),
                name: "read_config".to_string(),
                description: "Read orchestrator configuration snapshot.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "modify_skill".to_string(),
                name: "modify_skill".to_string(),
                description:
                    "Modify a managed skill definition (admin trigger required). args: {skill_id, patch}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string"},
                        "patch": {"type": "string"}
                    },
                    "required": ["skill_id", "patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
        ]
    }

    fn record_invocation(
        inner: &mut System0BrokerInner,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        output: &ToolOutput,
    ) {
        let (status, rendered_output) = match output {
            ToolOutput::Success(value) => ("success".to_string(), value.clone()),
            ToolOutput::Error(err) => ("error".to_string(), serde_json::json!({ "error": err })),
        };

        inner
            .tool_invocations_by_turn
            .entry(turn_id)
            .or_default()
            .push(OrchestratorToolInvocation {
                call_id: call.id.clone(),
                tool_id: call.tool_id.clone(),
                arguments: call.arguments.clone(),
                status,
                output: rendered_output,
            });
    }

    fn record_invocation_err(
        inner: &mut System0BrokerInner,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        err: &NeuromancerError,
    ) {
        Self::record_invocation(inner, turn_id, call, &ToolOutput::Error(err.to_string()));
    }
}

#[async_trait::async_trait]
impl ToolBroker for System0ToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let inner = self.inner.lock().await;
        let allowed_from_context: HashSet<String> = if ctx.allowed_tools.is_empty() {
            inner.allowlisted_tools.clone()
        } else {
            ctx.allowed_tools.iter().cloned().collect()
        };

        Self::build_tool_specs()
            .into_iter()
            .filter(|spec| {
                inner.allowlisted_tools.contains(&spec.id)
                    && allowed_from_context.contains(&spec.id)
            })
            .collect()
    }

    async fn call_tool(
        &self,
        _ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.current_turn_id;

        if !inner.allowlisted_tools.contains(&call.tool_id) {
            let err = NeuromancerError::Tool(ToolError::NotFound {
                tool_id: call.tool_id.clone(),
            });
            Self::record_invocation_err(&mut inner, turn_id, &call, &err);
            return Err(err);
        }

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
            "modify_skill" => {
                if inner.current_trigger_type != TriggerType::Admin {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(
                            "modify_skill requires an admin trigger".to_string(),
                        ),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }

                let Some(skill_id) = call
                    .arguments
                    .get("skill_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "modify_skill".to_string(),
                        message: "missing 'skill_id'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "status": "accepted",
                        "skill_id": skill_id,
                    })),
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

                let run_id = uuid::Uuid::new_v4().to_string();
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
                    })
                    .await
                {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = ?err,
                        "thread_journal_write_failed"
                    );
                } else {
                    // Account for the manually persisted user instruction so we do not
                    // persist the same user message again when writing conversation deltas.
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
                    .execute_turn(
                        &session_store,
                        session_id,
                        TriggerSource::Internal,
                        instruction.clone(),
                    )
                    .await;

                let mut error = None;
                let mut persisted_message_count = persisted_count_before_delta;
                let (run_state, summary) = match result {
                    Ok(turn_output) => {
                        let response = extract_response_text(&turn_output.output)
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
                    ToolOutput::Error(
                        error
                            .clone()
                            .unwrap_or_else(|| "delegation failed".to_string()),
                    )
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

                if let Some(snapshot) = thread_summary {
                    if let Err(err) = thread_journal.append_index_snapshot(&snapshot).await {
                        tracing::error!(
                            thread_id = %snapshot.thread_id,
                            run_id = %run_id,
                            error = ?err,
                            "thread_journal_index_write_failed"
                        );
                    }
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
                let err = NeuromancerError::Tool(ToolError::NotFound {
                    tool_id: call.tool_id.clone(),
                });
                Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                Err(err)
            }
        }
    }
}

fn map_xdg_err(err: neuromancer_core::xdg::XdgError) -> MessageRuntimeError {
    MessageRuntimeError::Config(err.to_string())
}

fn default_system0_tools() -> Vec<String> {
    vec![
        "delegate_to_agent".to_string(),
        "list_agents".to_string(),
        "read_config".to_string(),
        "modify_skill".to_string(),
    ]
}

fn effective_system0_tool_allowlist(configured: &[String]) -> Vec<String> {
    if configured.is_empty() {
        default_system0_tools()
    } else {
        configured.to_vec()
    }
}

fn load_system_prompt_file(path: &Path) -> Result<String, MessageRuntimeError> {
    validate_markdown_prompt_file(path).map_err(map_xdg_err)?;
    fs::read_to_string(path).map_err(|err| {
        MessageRuntimeError::Config(format!(
            "failed to read system prompt '{}': {err}",
            path.display()
        ))
    })
}

fn render_orchestrator_prompt(
    template: &str,
    mut agents: Vec<String>,
    mut tools: Vec<String>,
) -> String {
    agents.sort();
    tools.sort();
    let rendered_agents = if agents.is_empty() {
        "none".to_string()
    } else {
        agents.join(", ")
    };
    let rendered_tools = if tools.is_empty() {
        "none".to_string()
    } else {
        tools.join(", ")
    };
    template
        .replace("{{ORCHESTRATOR_ID}}", SYSTEM0_AGENT_ID)
        .replace("{{AVAILABLE_AGENTS}}", &rendered_agents)
        .replace("{{AVAILABLE_TOOLS}}", &rendered_tools)
}

fn resolve_tool_call_retry_limit(config: &NeuromancerConfig, agent_config: &AgentConfig) -> u32 {
    let slot_name = agent_config
        .models
        .executor
        .as_deref()
        .unwrap_or("executor");
    config
        .models
        .get(slot_name)
        .map(|slot| slot.tool_call_retry_limit)
        .unwrap_or(1)
}

fn build_llm_client(
    config: &NeuromancerConfig,
    agent_config: &AgentConfig,
) -> Result<Arc<dyn LlmClient>, MessageRuntimeError> {
    let slot_name = agent_config
        .models
        .executor
        .as_deref()
        .unwrap_or("executor");
    let Some(slot) = config.models.get(slot_name) else {
        return Ok(Arc::new(EchoLlmClient));
    };

    match slot.provider.as_str() {
        "groq" => {
            let key = std::env::var("GROQ_API_KEY").map_err(|_| {
                MessageRuntimeError::Config(
                    "GROQ_API_KEY is required when using provider='groq'".to_string(),
                )
            })?;
            let groq_compat =
                rig::providers::openai::Client::from_url(&key, "https://api.groq.com/openai/v1");
            Ok(Arc::new(RigLlmClient::new(
                groq_compat.completion_model(&slot.model),
            )))
        }
        "mock" => Ok(Arc::new(TwoStepMockLlmClient::default())),
        other => Err(MessageRuntimeError::Config(format!(
            "agent '{}' uses unsupported model provider '{}'",
            agent_config.id, other
        ))),
    }
}

#[derive(Default)]
struct TwoStepMockLlmClient {
    issued_tools: std::sync::Mutex<bool>,
}

#[async_trait::async_trait]
impl LlmClient for TwoStepMockLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<neuromancer_agent::llm::LlmResponse, NeuromancerError> {
        let mut issued_tools = self.issued_tools.lock().map_err(|_| {
            NeuromancerError::Infra(neuromancer_core::error::InfraError::Config(
                "mock llm lock poisoned".to_string(),
            ))
        })?;

        if !*issued_tools && !tool_definitions.is_empty() {
            *issued_tools = true;
            let has_delegate_tool = tool_definitions
                .iter()
                .any(|tool| tool.name == "delegate_to_agent");
            let calls = if has_delegate_tool {
                vec![mock_delegate_call(&messages)]
            } else {
                tool_definitions
                    .iter()
                    .enumerate()
                    .map(|(idx, tool)| ToolCall {
                        id: format!("mock-call-{}", idx + 1),
                        tool_id: tool.name.clone(),
                        arguments: serde_json::json!({}),
                    })
                    .collect()
            };

            return Ok(neuromancer_agent::llm::LlmResponse {
                text: None,
                tool_calls: calls,
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        *issued_tools = false;
        if let Some(summary) = mock_finance_summary_from_messages(&messages) {
            return Ok(neuromancer_agent::llm::LlmResponse {
                text: Some(summary),
                tool_calls: vec![],
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        Ok(neuromancer_agent::llm::LlmResponse {
            text: Some("System0 turn completed.".to_string()),
            tool_calls: vec![],
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }
}

fn mock_delegate_call(messages: &[rig::completion::Message]) -> ToolCall {
    let user_text = mock_last_user_text(messages).to_ascii_lowercase();
    let is_finance_request = user_text.contains("finance")
        || user_text.contains("bill")
        || user_text.contains("account");

    if is_finance_request {
        ToolCall {
            id: "mock-call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            arguments: serde_json::json!({
                "agent_id": "finance-manager",
                "instruction": "Use manage-bills and manage-accounts to answer with due totals and available balance."
            }),
        }
    } else {
        ToolCall {
            id: "mock-call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            arguments: serde_json::json!({
                "agent_id": "planner",
                "instruction": "Summarize the user request"
            }),
        }
    }
}

fn mock_last_user_text(messages: &[rig::completion::Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|msg| match msg {
            rig::completion::Message::User { content } => {
                content.iter().find_map(|part| match part {
                    rig::message::UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn mock_finance_summary_from_messages(messages: &[rig::completion::Message]) -> Option<String> {
    let mut total_due = None;
    let mut total_balance = None;

    for msg in messages {
        let rig::completion::Message::User { content } = msg else {
            continue;
        };

        for part in content.iter() {
            let rig::message::UserContent::ToolResult(result) = part else {
                continue;
            };

            let text = result
                .content
                .iter()
                .find_map(|item| match item {
                    rig::message::ToolResultContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                continue;
            };

            match value.get("skill").and_then(|skill| skill.as_str()) {
                Some("manage-bills") => {
                    total_due = value
                        .get("script_result")
                        .and_then(|result| result.get("total_due"))
                        .and_then(|amount| amount.as_f64());
                }
                Some("manage-accounts") => {
                    total_balance = value
                        .get("script_result")
                        .and_then(|result| result.get("total_balance"))
                        .and_then(|amount| amount.as_f64())
                        .or_else(|| {
                            value
                                .get("csv")
                                .and_then(|csv| csv.as_array())
                                .and_then(|docs| docs.first())
                                .and_then(|doc| doc.get("summary"))
                                .and_then(|summary| summary.get("total_balance"))
                                .and_then(|amount| amount.as_f64())
                        });
                }
                _ => {}
            }
        }
    }

    match (total_due, total_balance) {
        (Some(due), Some(balance)) => Some(format!(
            "Finance snapshot: total_due=${:.2}, total_balance=${:.2}, remaining=${:.2}.",
            due,
            balance,
            balance - due
        )),
        _ => None,
    }
}

struct EchoLlmClient;

#[async_trait::async_trait]
impl LlmClient for EchoLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        _tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<neuromancer_agent::llm::LlmResponse, NeuromancerError> {
        let fallback = messages
            .iter()
            .rev()
            .find_map(|msg| match msg {
                rig::completion::Message::User { content } => content.iter().find_map(|part| {
                    if let rig::message::UserContent::Text(text) = part {
                        Some(text.text.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .unwrap_or_else(|| "No message provided.".to_string());

        Ok(neuromancer_agent::llm::LlmResponse {
            text: Some(format!("Echo: {fallback}")),
            tool_calls: vec![],
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }
}

#[derive(Clone)]
struct SkillToolBroker {
    tools: HashMap<String, SkillTool>,
    tool_aliases: HashMap<String, String>,
    aliases_by_tool: HashMap<String, Vec<String>>,
    local_root: PathBuf,
}

#[derive(Clone)]
struct SkillTool {
    description: String,
    skill_root: PathBuf,
    markdown_paths: Vec<String>,
    csv_paths: Vec<String>,
    script_path: Option<String>,
    script_timeout: Duration,
}

impl SkillToolBroker {
    fn new(
        agent_id: &str,
        allowed_skills: &[String],
        skill_registry: &SkillRegistry,
        local_root: PathBuf,
    ) -> Result<Self, MessageRuntimeError> {
        let mut tools = HashMap::new();
        for skill_name in allowed_skills {
            let skill = skill_registry.get(skill_name).ok_or_else(|| {
                MessageRuntimeError::Config(format!(
                    "agent '{}' references missing skill '{}'",
                    agent_id, skill_name
                ))
            })?;

            tools.insert(skill_name.clone(), skill_tool_from_skill(skill));
        }

        let (tool_aliases, aliases_by_tool) = build_skill_tool_aliases(allowed_skills);

        Ok(Self {
            tools,
            tool_aliases,
            aliases_by_tool,
            local_root,
        })
    }

    fn resolve_tool_id(&self, ctx: &AgentContext, requested_tool_id: &str) -> Option<String> {
        let is_allowed = |tool_id: &str| ctx.allowed_tools.iter().any(|allowed| allowed == tool_id);
        if is_allowed(requested_tool_id) && self.tools.contains_key(requested_tool_id) {
            return Some(requested_tool_id.to_string());
        }

        let canonical = self.tool_aliases.get(requested_tool_id)?;
        if is_allowed(canonical) && self.tools.contains_key(canonical) {
            Some(canonical.clone())
        } else {
            None
        }
    }
}

#[async_trait::async_trait]
impl ToolBroker for SkillToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let mut specs = Vec::new();
        for name in &ctx.allowed_tools {
            let Some(tool) = self.tools.get(name) else {
                continue;
            };

            specs.push(skill_tool_spec(name, name, &tool.description));

            if let Some(aliases) = self.aliases_by_tool.get(name) {
                for alias in aliases {
                    specs.push(skill_tool_spec(
                        alias,
                        name,
                        &format!("Alias for '{name}'. {}", tool.description),
                    ));
                }
            }
        }

        specs
    }

    async fn call_tool(
        &self,
        ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let started_at = Instant::now();
        let requested_tool_id = call.tool_id.clone();
        tracing::info!(
            agent_id = %ctx.agent_id,
            task_id = %ctx.task_id,
            tool_id = %requested_tool_id,
            call_id = %call.id,
            "skill_tool_started"
        );

        let Some(canonical_tool_id) = self.resolve_tool_id(ctx, &requested_tool_id) else {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: requested_tool_id,
            }));
        };

        if canonical_tool_id != requested_tool_id {
            tracing::warn!(
                agent_id = %ctx.agent_id,
                task_id = %ctx.task_id,
                requested_tool_id = %requested_tool_id,
                canonical_tool_id = %canonical_tool_id,
                "skill_tool_alias_used"
            );
        }

        let Some(tool) = self.tools.get(&canonical_tool_id) else {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: canonical_tool_id,
            }));
        };

        let mut markdown_docs = Vec::new();
        for raw in &tool.markdown_paths {
            let path = resolve_local_data_path(&self.local_root, raw)
                .map_err(map_tool_err(&canonical_tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: canonical_tool_id.clone(),
                    message: err.to_string(),
                })
            })?;
            markdown_docs.push(serde_json::json!({
                "path": raw,
                "content": content,
            }));
        }

        let mut csv_docs = Vec::new();
        for raw in &tool.csv_paths {
            let path = resolve_local_data_path(&self.local_root, raw)
                .map_err(map_tool_err(&canonical_tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: canonical_tool_id.clone(),
                    message: err.to_string(),
                })
            })?;
            let parsed =
                parse_csv_content(&path, &content).map_err(map_tool_err(&canonical_tool_id))?;
            csv_docs.push(serde_json::json!({
                "path": raw,
                "headers": parsed.headers,
                "rows": parsed.rows,
                "summary": {
                    "row_count": parsed.row_count,
                    "total_balance": parsed.total_balance,
                }
            }));
        }

        let script_result = if let Some(relative_script_path) = tool.script_path.as_deref() {
            let script_path = resolve_skill_script_path(&tool.skill_root, relative_script_path)
                .map_err(map_tool_err(&canonical_tool_id))?;

            let payload = serde_json::json!({
                "local_root": self.local_root.display().to_string(),
                "skill": canonical_tool_id.clone(),
                "data_sources": {
                    "markdown": tool.markdown_paths.clone(),
                    "csv": tool.csv_paths.clone(),
                },
                "arguments": call.arguments.clone(),
            });

            Some(
                run_skill_script(
                    &script_path,
                    &payload,
                    tool.script_timeout,
                    &ctx.agent_id,
                    &ctx.task_id.to_string(),
                    &canonical_tool_id,
                )
                .await
                .map_err(map_tool_err(&canonical_tool_id))?,
            )
        } else {
            None
        };

        tracing::info!(
            agent_id = %ctx.agent_id,
            task_id = %ctx.task_id,
            tool_id = %requested_tool_id,
            skill_id = %canonical_tool_id,
            call_id = %call.id,
            duration_ms = started_at.elapsed().as_millis(),
            "skill_tool_finished"
        );

        Ok(ToolResult {
            call_id: call.id,
            output: ToolOutput::Success(serde_json::json!({
                "skill": canonical_tool_id,
                "markdown": markdown_docs,
                "csv": csv_docs,
                "script_result": script_result,
            })),
        })
    }
}

fn map_tool_err(tool_id: &str) -> impl Fn(MessageRuntimeError) -> NeuromancerError + '_ {
    move |err| {
        NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: tool_id.to_string(),
            message: err.to_string(),
        })
    }
}

fn skill_tool_from_skill(skill: &Skill) -> SkillTool {
    let metadata = &skill.metadata;
    SkillTool {
        description: if metadata.description.trim().is_empty() {
            format!("Skill {}", metadata.name)
        } else {
            metadata.description.clone()
        },
        skill_root: skill.path.clone(),
        markdown_paths: metadata.data_sources.markdown.clone(),
        csv_paths: metadata.data_sources.csv.clone(),
        script_path: metadata.execution.script.clone(),
        script_timeout: metadata
            .execution
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_SKILL_SCRIPT_TIMEOUT),
    }
}

fn skill_tool_spec(name: &str, skill_id: &str, description: &str) -> ToolSpec {
    ToolSpec {
        id: name.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        source: ToolSource::Skill {
            skill_id: skill_id.to_string(),
        },
    }
}

fn build_skill_tool_aliases(
    allowed_skills: &[String],
) -> (HashMap<String, String>, HashMap<String, Vec<String>>) {
    let canonical_skills: HashSet<&str> = allowed_skills.iter().map(String::as_str).collect();
    let mut alias_to_canonical = HashMap::<String, String>::new();
    let mut aliases_by_tool = HashMap::<String, Vec<String>>::new();

    for canonical in allowed_skills {
        let alias = canonical.replace('-', "_");
        if alias == *canonical {
            continue;
        }

        // Keep canonical names authoritative when an underscore variant already exists.
        if canonical_skills.contains(alias.as_str()) {
            continue;
        }

        if alias_to_canonical.contains_key(&alias) {
            continue;
        }

        alias_to_canonical.insert(alias.clone(), canonical.clone());
        aliases_by_tool
            .entry(canonical.clone())
            .or_default()
            .push(alias);
    }

    (alias_to_canonical, aliases_by_tool)
}

fn resolve_local_data_path(
    local_root: &Path,
    relative: &str,
) -> Result<PathBuf, MessageRuntimeError> {
    resolve_relative_path_under_root(local_root, relative, "data file")
}

fn resolve_skill_script_path(
    skill_root: &Path,
    relative: &str,
) -> Result<PathBuf, MessageRuntimeError> {
    resolve_relative_path_under_root(skill_root, relative, "skill script")
}

fn resolve_relative_path_under_root(
    root: &Path,
    relative: &str,
    file_type: &str,
) -> Result<PathBuf, MessageRuntimeError> {
    let input = Path::new(relative);
    if input.is_absolute() {
        return Err(MessageRuntimeError::PathViolation(format!(
            "absolute paths are not allowed: {relative}"
        )));
    }

    for component in input.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(MessageRuntimeError::PathViolation(format!(
                "path traversal is not allowed: {relative}"
            )));
        }
    }

    let root_canonical = fs::canonicalize(root).map_err(|err| {
        MessageRuntimeError::ResourceNotFound(format!(
            "{} root '{}' is unavailable: {err}",
            file_type,
            root.display()
        ))
    })?;

    let full_path = root.join(input);
    let target_canonical = fs::canonicalize(&full_path).map_err(|err| {
        MessageRuntimeError::ResourceNotFound(format!(
            "{} '{}' is unavailable: {err}",
            file_type,
            full_path.display()
        ))
    })?;

    if !target_canonical.starts_with(&root_canonical) {
        return Err(MessageRuntimeError::PathViolation(format!(
            "resolved path '{}' escapes '{}'",
            target_canonical.display(),
            root_canonical.display()
        )));
    }

    Ok(target_canonical)
}

fn script_runtime_error(kind: &str, message: impl Into<String>) -> MessageRuntimeError {
    MessageRuntimeError::Internal(format!("script_{kind}: {}", message.into()))
}

async fn run_skill_script(
    script_path: &Path,
    payload: &serde_json::Value,
    timeout: Duration,
    agent_id: &str,
    task_id: &str,
    tool_id: &str,
) -> Result<serde_json::Value, MessageRuntimeError> {
    let started_at = Instant::now();
    tracing::info!(
        agent_id = %agent_id,
        task_id = %task_id,
        tool_id = %tool_id,
        script_path = %script_path.display(),
        timeout_ms = timeout.as_millis(),
        "skill_script_started"
    );

    let stdin_payload = serde_json::to_vec(payload).map_err(|err| {
        script_runtime_error(
            "io_error",
            format!("failed to encode script input payload: {err}"),
        )
    })?;

    let mut child = TokioCommand::new("python3")
        .arg("-I")
        .arg(script_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed to start python script '{}': {err}",
                    script_path.display()
                ),
            )
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        script_runtime_error(
            "io_error",
            format!(
                "script stdin is unavailable for '{}'",
                script_path.display()
            ),
        )
    })?;
    stdin.write_all(&stdin_payload).await.map_err(|err| {
        script_runtime_error(
            "io_error",
            format!(
                "failed to write input to script '{}': {err}",
                script_path.display()
            ),
        )
    })?;
    drop(stdin);

    let mut stdout = child.stdout.take().ok_or_else(|| {
        script_runtime_error(
            "io_error",
            format!(
                "script stdout is unavailable for '{}'",
                script_path.display()
            ),
        )
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        script_runtime_error(
            "io_error",
            format!(
                "script stderr is unavailable for '{}'",
                script_path.display()
            ),
        )
    })?;

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).await.map(|_| buffer)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await.map(|_| buffer)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed waiting for script '{}': {err}",
                    script_path.display()
                ),
            )
        })?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(script_runtime_error(
                "timeout",
                format!(
                    "script '{}' exceeded timeout of {}ms",
                    script_path.display(),
                    timeout.as_millis()
                ),
            ));
        }
    };

    let stdout_bytes = stdout_task
        .await
        .map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed joining stdout reader for '{}': {err}",
                    script_path.display()
                ),
            )
        })?
        .map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed reading script stdout '{}': {err}",
                    script_path.display()
                ),
            )
        })?;
    let stderr_bytes = stderr_task
        .await
        .map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed joining stderr reader for '{}': {err}",
                    script_path.display()
                ),
            )
        })?
        .map_err(|err| {
            script_runtime_error(
                "io_error",
                format!(
                    "failed reading script stderr '{}': {err}",
                    script_path.display()
                ),
            )
        })?;

    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if !status.success() {
        return Err(script_runtime_error(
            "io_error",
            format!(
                "script '{}' exited with status {}: {}",
                script_path.display(),
                status,
                stderr.trim()
            ),
        ));
    }

    let stdout = String::from_utf8(stdout_bytes).map_err(|err| {
        script_runtime_error(
            "invalid_json",
            format!(
                "script '{}' emitted non-utf8 stdout: {err}",
                script_path.display()
            ),
        )
    })?;
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|err| {
        script_runtime_error(
            "invalid_json",
            format!(
                "script '{}' emitted invalid JSON: {err}; stderr='{}'",
                script_path.display(),
                stderr.trim()
            ),
        )
    })?;

    tracing::info!(
        agent_id = %agent_id,
        task_id = %task_id,
        tool_id = %tool_id,
        script_path = %script_path.display(),
        duration_ms = started_at.elapsed().as_millis(),
        status = %status,
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "skill_script_finished"
    );

    Ok(parsed)
}

struct ParsedCsv {
    headers: Vec<String>,
    rows: Vec<HashMap<String, String>>,
    row_count: usize,
    total_balance: f64,
}

fn parse_csv_content(path: &Path, content: &str) -> Result<ParsedCsv, MessageRuntimeError> {
    let mut lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());

    let Some(header_line) = lines.next() else {
        return Err(MessageRuntimeError::ResourceNotFound(format!(
            "csv file is empty: {}",
            path.display()
        )));
    };

    let headers: Vec<String> = header_line
        .split(',')
        .map(|value| value.trim().to_string())
        .collect();

    if headers.is_empty() {
        return Err(MessageRuntimeError::Config(format!(
            "csv file has no headers: {}",
            path.display()
        )));
    }

    let mut rows = Vec::new();
    let mut total_balance = 0.0_f64;
    for line in lines {
        let values: Vec<String> = line
            .split(',')
            .map(|value| value.trim().to_string())
            .collect();

        if values.len() != headers.len() {
            return Err(MessageRuntimeError::Config(format!(
                "csv row has {} columns but expected {} in {}",
                values.len(),
                headers.len(),
                path.display()
            )));
        }

        let mut row = HashMap::new();
        for (idx, header) in headers.iter().enumerate() {
            let value = values[idx].clone();
            if header.eq_ignore_ascii_case("balance") {
                total_balance += parse_numeric_value(&value);
            }
            row.insert(header.clone(), value);
        }
        rows.push(row);
    }

    Ok(ParsedCsv {
        row_count: rows.len(),
        headers,
        rows,
        total_balance,
    })
}

fn parse_numeric_value(value: &str) -> f64 {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
        .collect();
    cleaned.parse::<f64>().unwrap_or(0.0)
}

fn extract_response_text(output: &neuromancer_core::task::TaskOutput) -> Option<String> {
    output
        .artifacts
        .first()
        .map(|artifact| artifact.content.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn python3_available() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    #[test]
    fn resolve_local_data_path_rejects_absolute_paths() {
        let local_root = temp_dir("nm_local_root");
        let absolute = local_root.join("data/accounts.csv");
        let err = resolve_local_data_path(&local_root, absolute.to_string_lossy().as_ref())
            .expect_err("absolute path should be rejected");
        assert!(matches!(err, MessageRuntimeError::PathViolation(_)));
    }

    #[test]
    fn resolve_local_data_path_rejects_parent_traversal() {
        let local_root = temp_dir("nm_local_root");
        let err = resolve_local_data_path(&local_root, "../secrets.txt")
            .expect_err("parent traversal should be rejected");
        assert!(matches!(err, MessageRuntimeError::PathViolation(_)));
    }

    #[test]
    fn resolve_local_data_path_accepts_valid_relative_path() {
        let local_root = temp_dir("nm_local_root");
        let data_dir = local_root.join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        let file_path = data_dir.join("accounts.csv");
        fs::write(&file_path, "account,balance\nchecking,1200").expect("write");

        let resolved = resolve_local_data_path(&local_root, "data/accounts.csv")
            .expect("relative path should resolve");
        assert_eq!(resolved, fs::canonicalize(file_path).expect("canonical"));
    }

    #[test]
    fn resolve_skill_script_path_rejects_absolute_paths() {
        let skill_root = temp_dir("nm_skill_root");
        let absolute = skill_root.join("scripts/run.py");
        let err = resolve_skill_script_path(&skill_root, absolute.to_string_lossy().as_ref())
            .expect_err("absolute path should be rejected");
        assert!(matches!(err, MessageRuntimeError::PathViolation(_)));
    }

    #[test]
    fn resolve_skill_script_path_rejects_parent_traversal() {
        let skill_root = temp_dir("nm_skill_root");
        let err = resolve_skill_script_path(&skill_root, "../run.py")
            .expect_err("parent traversal should be rejected");
        assert!(matches!(err, MessageRuntimeError::PathViolation(_)));
    }

    #[test]
    fn resolve_skill_script_path_accepts_valid_relative_path() {
        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("run.py");
        fs::write(&script, "print('{}')").expect("script write");

        let resolved = resolve_skill_script_path(&skill_root, "scripts/run.py")
            .expect("relative script path should resolve");
        assert_eq!(resolved, fs::canonicalize(script).expect("canonical"));
    }

    #[tokio::test]
    async fn run_skill_script_returns_json() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("run.py");
        fs::write(
            &script,
            r#"import json, sys
payload = json.loads(sys.stdin.read())
print(json.dumps({"ok": True, "skill": payload.get("skill")}))
"#,
        )
        .expect("script write");

        let payload = serde_json::json!({
            "skill": "manage-bills",
            "arguments": {}
        });
        let value = run_skill_script(
            &script,
            &payload,
            Duration::from_secs(1),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect("script should succeed");
        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(value["skill"], serde_json::json!("manage-bills"));
    }

    #[tokio::test]
    async fn run_skill_script_times_out() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("slow.py");
        fs::write(
            &script,
            r#"import time
time.sleep(1.0)
print("{}")
"#,
        )
        .expect("script write");

        let err = run_skill_script(
            &script,
            &serde_json::json!({}),
            Duration::from_millis(10),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect_err("script should time out");
        assert!(
            err.to_string().contains("script_timeout"),
            "unexpected timeout error: {err}"
        );
    }

    #[tokio::test]
    async fn run_skill_script_rejects_invalid_json() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("invalid.py");
        fs::write(&script, r#"print("not-json")"#).expect("script write");

        let err = run_skill_script(
            &script,
            &serde_json::json!({}),
            Duration::from_secs(1),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect_err("script output should fail JSON parsing");
        assert!(
            err.to_string().contains("script_invalid_json"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn build_skill_tool_aliases_adds_underscore_variants() {
        let (alias_to_canonical, aliases_by_tool) =
            build_skill_tool_aliases(&["manage-bills".to_string(), "manage-accounts".to_string()]);

        assert_eq!(
            alias_to_canonical.get("manage_bills"),
            Some(&"manage-bills".to_string())
        );
        assert_eq!(
            alias_to_canonical.get("manage_accounts"),
            Some(&"manage-accounts".to_string())
        );
        assert_eq!(
            aliases_by_tool.get("manage-bills"),
            Some(&vec!["manage_bills".to_string()])
        );
        assert_eq!(
            aliases_by_tool.get("manage-accounts"),
            Some(&vec!["manage_accounts".to_string()])
        );
    }

    #[test]
    fn build_skill_tool_aliases_skips_conflicting_names() {
        let (alias_to_canonical, aliases_by_tool) =
            build_skill_tool_aliases(&["manage-bills".to_string(), "manage_bills".to_string()]);

        assert!(!alias_to_canonical.contains_key("manage_bills"));
        assert!(!aliases_by_tool.contains_key("manage-bills"));
    }

    #[test]
    fn resolve_tool_call_retry_limit_uses_model_slot_value() {
        let config_toml = r#"
[global]
instance_id = "test"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-model"
tool_call_retry_limit = 4

[orchestrator]
model_slot = "executor"

[agents.planner]
models.executor = "executor"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []
"#;
        let config: NeuromancerConfig = toml::from_str(config_toml).expect("config parse");
        let planner = config.agents.get("planner").expect("planner config");
        let agent_config = planner.to_agent_config("planner", "system prompt".to_string());
        assert_eq!(resolve_tool_call_retry_limit(&config, &agent_config), 4);
    }

    #[test]
    fn render_orchestrator_prompt_expands_placeholders() {
        let rendered = render_orchestrator_prompt(
            "id={{ORCHESTRATOR_ID}} agents={{AVAILABLE_AGENTS}} tools={{AVAILABLE_TOOLS}}",
            vec!["planner".into(), "browser".into()],
            vec!["read_config".into(), "list_agents".into()],
        );
        assert!(rendered.contains("id=system0"));
        assert!(rendered.contains("agents=browser, planner"));
        assert!(rendered.contains("tools=list_agents, read_config"));
    }

    #[test]
    fn render_orchestrator_prompt_handles_empty_lists() {
        let rendered = render_orchestrator_prompt(
            "agents={{AVAILABLE_AGENTS}} tools={{AVAILABLE_TOOLS}}",
            vec![],
            vec![],
        );
        assert!(rendered.contains("agents=none"));
        assert!(rendered.contains("tools=none"));
    }
}
