use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::memory::MemoryStore;
use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorSubagentTurnResult, OrchestratorTurnResult, ThreadSummary,
};
use neuromancer_core::thread::{AgentThread, CompactionPolicy, ThreadScope, ThreadStatus};
use neuromancer_core::task::{Task, TaskId, TaskOutput};
use neuromancer_core::tool::{AgentContext, ToolBroker, ToolCall, ToolResult, ToolSpec};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_core::xdg::XdgLayout;
use tokio::sync::{mpsc, oneshot};

use std::str::FromStr;

use crate::orchestrator::actions::dispatch::dispatch_tool;
use crate::orchestrator::bootstrap::map_xdg_err;
use crate::orchestrator::error::System0Error;
use crate::orchestrator::state::{ActiveRunContext, System0ToolBroker, TaskManager, TaskStore};
use crate::orchestrator::threads::SqliteThreadStore;
use crate::orchestrator::tools::effective_system0_tool_allowlist;
use crate::orchestrator::tracing::conversation_projection::normalize_error_message;
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, make_event, now_rfc3339};

mod builder;
mod rpc_queries;
pub(super) mod turn_worker;

pub struct System0Runtime {
    turn_tx: mpsc::Sender<TurnRequest>,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
    _task_worker: tokio::task::JoinHandle<()>,
    _watchdog_worker: tokio::task::JoinHandle<()>,
    _turn_worker: tokio::task::JoinHandle<()>,
    _report_worker: tokio::task::JoinHandle<()>,
}

struct TurnRequest {
    message: String,
    trigger_type: TriggerType,
    response_tx: oneshot::Sender<Result<OrchestratorTurnResult, System0Error>>,
}

impl System0Runtime {
    pub async fn new(config: &NeuromancerConfig, config_path: &Path) -> Result<Self, System0Error> {
        let layout = XdgLayout::from_env().map_err(map_xdg_err)?;
        let local_root = layout.runtime_root();
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new(".")) // TODO: is this a bad fallback?
            .to_path_buf();

        let thread_journal = ThreadJournal::new(layout.runtime_root().join("threads"))?;
        let (skill_registry, execution_guard) =
            builder::resolve_environment(&layout, config).await?;
        let known_skill_ids = skill_registry.list_names();

        let task_store = TaskStore::open(&layout.runtime_root().join("orchestrator.sqlite"))
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?;
        let task_manager = TaskManager::new(task_store)
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?;
        for (agent_id, agent_cfg) in &config.agents {
            task_manager
                .register_agent_health(agent_id.clone(), agent_cfg.health.clone())
                .await;
        }
        let (report_tx, mut report_rx) = mpsc::channel(256);

        let thread_store = SqliteThreadStore::open(&layout.runtime_root().join("threads.sqlite"))
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?;
        let thread_store: Arc<dyn neuromancer_core::thread::ThreadStore> =
            Arc::new(thread_store);

        let allowlisted_system0_tools =
            effective_system0_tool_allowlist(&config.orchestrator.capabilities.skills);
        let subagents = builder::build_subagents(
            config,
            &layout,
            &config_dir,
            &skill_registry,
            &local_root,
            &execution_guard,
            &report_tx,
            &task_manager,
            &thread_store,
        )?;
        let config_snapshot =
            serde_json::to_value(config).map_err(|err| System0Error::Config(err.to_string()))?;

        let memory_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::from_str(
                    &format!("sqlite://{}", layout.runtime_root().join("memory.sqlite").display()),
                )
                .map_err(|err| System0Error::Config(err.to_string()))?
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
            )
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?;
        let memory_store: Arc<dyn MemoryStore> =
            Arc::new(neuromancer_memory_simple::SqliteMemoryStore::new(memory_pool)
                .await
                .map_err(|err| System0Error::Config(err.to_string()))?);

        // Ensure System0's AgentThread exists.
        let system0_thread_id = crate::orchestrator::state::SYSTEM0_AGENT_ID.to_string();
        if thread_store
            .get_thread(&system0_thread_id)
            .await
            .map_err(|err| System0Error::Config(err.to_string()))?
            .is_none()
        {
            let now = chrono::Utc::now();
            let system0_thread = AgentThread {
                id: system0_thread_id.clone(),
                agent_id: crate::orchestrator::state::SYSTEM0_AGENT_ID.to_string(),
                scope: ThreadScope::System0,
                compaction_policy: CompactionPolicy::SummarizeToMemory {
                    target_partition: "system0".to_string(),
                    summarizer_model: "default".to_string(),
                    threshold_pct: 0.8,
                },
                context_window_budget: 128_000,
                status: ThreadStatus::Active,
                created_at: now,
                updated_at: now,
            };
            thread_store
                .create_thread(&system0_thread)
                .await
                .map_err(|err| System0Error::Config(err.to_string()))?;
        }

        let system0_broker = System0ToolBroker::new(
            subagents,
            config_snapshot,
            &allowlisted_system0_tools,
            thread_store.clone(),
            memory_store,
            thread_journal.clone(),
            config.orchestrator.self_improvement.clone(),
            &known_skill_ids,
            execution_guard,
            task_manager,
        );
        let runtime_broker = system0_broker.clone();
        let report_broker = system0_broker.clone();
        let report_worker = tokio::spawn(async move {
            while let Some(report) = report_rx.recv().await {
                if let Err(err) = report_broker.ingest_subagent_report(report).await {
                    tracing::error!(error = ?err, "subagent_report_ingest_failed");
                }
            }
        });

        let system0_agent_runtime = builder::build_system0_agent(
            config,
            &layout,
            &config_dir,
            &allowlisted_system0_tools,
            &system0_broker,
            report_tx,
        )?;

        let memory_store_for_worker = system0_broker.memory_store().await;
        let (turn_tx, turn_worker) = builder::spawn_turn_worker(
            system0_agent_runtime,
            thread_store,
            memory_store_for_worker,
            system0_thread_id,
            system0_broker,
            thread_journal.clone(),
        );
        let task_worker = builder::spawn_task_worker(runtime_broker.clone());
        let watchdog_worker = builder::spawn_watchdog_worker(runtime_broker.clone());

        Ok(Self {
            turn_tx,
            system0_broker: runtime_broker,
            thread_journal,
            _task_worker: task_worker,
            _watchdog_worker: watchdog_worker,
            _turn_worker: turn_worker,
            _report_worker: report_worker,
        })
    }

    pub async fn enqueue_direct(&self, task: Task) -> Result<TaskId, System0Error> {
        self.system0_broker
            .enqueue_direct(task)
            .await
            .map_err(|err| System0Error::Internal(err.to_string()))
    }

    pub async fn await_task_result(
        &self,
        task_id: TaskId,
        timeout: Duration,
    ) -> Result<TaskOutput, System0Error> {
        self.system0_broker
            .await_task_result(task_id, timeout)
            .await
            .map_err(|err| System0Error::Internal(err.to_string()))
    }

    pub async fn graceful_shutdown(&self, timeout: Duration) -> Result<(), System0Error> {
        self.system0_broker.stop_accepting_new_tasks().await;

        let started = Instant::now();
        loop {
            let (queued, running, _completed, _failed, _total) =
                self.system0_broker.task_queue_snapshot().await;
            if queued + running == 0 {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let suspended = self
            .system0_broker
            .suspend_unfinished_tasks("daemon shutdown")
            .await
            .map_err(|err| System0Error::Internal(err.to_string()))?;
        tracing::info!(suspended, "runtime_shutdown_suspended_tasks");
        Ok(())
    }

    pub async fn turn(&self, message: String) -> Result<OrchestratorTurnResult, System0Error> {
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

        let received = tokio::time::timeout(builder::TURN_TIMEOUT, response_rx)
            .await
            .map_err(|_| {
                System0Error::Timeout(humantime::format_duration(builder::TURN_TIMEOUT).to_string())
            })?;

        received.map_err(|_| System0Error::Unavailable("turn worker stopped".to_string()))?
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
            .ok_or_else(|| System0Error::ResourceNotFound(format!("thread '{thread_id}'")))?;

        let runtime = self
            .system0_broker
            .runtime_for_agent(&state.agent_id)
            .await
            .ok_or_else(|| System0Error::ResourceNotFound(format!("agent '{}'", state.agent_id)))?;
        let thread_store = self.system0_broker.thread_store().await;
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
        #[allow(deprecated)]
        let run_result = runtime
            .execute_turn_with_thread_store(
                &*thread_store,
                &thread_id,
                TriggerSource::Internal,
                message.clone(),
                run_uuid,
                vec![],
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
            .record_subagent_turn_result(&thread_id, delegated_run.clone())
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
#[path = "../runtime_tests.rs"]
mod runtime_tests;
