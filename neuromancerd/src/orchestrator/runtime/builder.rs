use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::AgentError;
use neuromancer_core::task::TaskState;
use neuromancer_core::thread::{ThreadId, ThreadStore};
use neuromancer_core::tool::ToolBroker;
use neuromancer_core::xdg::{XdgLayout, resolve_path};
use neuromancer_skills::SkillRegistry;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::time::MissedTickBehavior;

use crate::orchestrator::bootstrap::{build_system0_agent_config, map_xdg_err};
use crate::orchestrator::error::System0Error;
use crate::orchestrator::llm_clients::{build_llm_client, resolve_tool_call_retry_limit};
use crate::orchestrator::prompt::{load_system_prompt_file, render_system0_prompt};
use crate::orchestrator::security::execution_guard::{ExecutionGuard, PlaceholderExecutionGuard};
use crate::orchestrator::skills::{OrchestratorSkillBroker, SkillToolBroker};
use crate::orchestrator::state::task_manager::running_state;
use crate::orchestrator::state::{
    System0ToolBroker, TaskManager, task_manager::task_failure_payload,
};
use crate::orchestrator::tracing::thread_journal::ThreadJournal;

use super::TurnRequest;
use super::turn_worker::System0TurnWorker;

pub(super) const TURN_TIMEOUT: Duration = Duration::from_secs(180);

pub(super) async fn resolve_environment(
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

pub(super) fn build_subagents(
    config: &NeuromancerConfig,
    layout: &XdgLayout,
    config_dir: &Path,
    skill_registry: &SkillRegistry,
    local_root: &Path,
    execution_guard: &Arc<dyn ExecutionGuard>,
    report_tx: &mpsc::Sender<neuromancer_core::agent::SubAgentReport>,
    task_manager: &TaskManager,
) -> Result<HashMap<String, Arc<AgentRuntime>>, System0Error> {
    let mut subagents = HashMap::new();
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
        let skill_broker = SkillToolBroker::new(
            agent_id,
            &agent_config.capabilities.skills,
            skill_registry,
            local_root.to_path_buf(),
            execution_guard.clone(),
        )
        .map_err(|e| System0Error::Config(e.to_string()))?;
        let broker: Arc<dyn ToolBroker> = Arc::new(OrchestratorSkillBroker::new(
            skill_broker,
            task_manager.clone(),
        ));

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

pub(super) fn build_system0_agent(
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

pub(super) fn spawn_turn_worker(
    system0_agent_runtime: Arc<AgentRuntime>,
    thread_store: Arc<dyn ThreadStore>,
    system0_thread_id: ThreadId,
    system0_broker: System0ToolBroker,
    thread_journal: ThreadJournal,
) -> (mpsc::Sender<TurnRequest>, tokio::task::JoinHandle<()>) {
    let core = Arc::new(AsyncMutex::new(System0TurnWorker {
        agent_runtime: system0_agent_runtime,
        thread_store,
        system0_thread_id,
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

pub(super) fn spawn_task_worker(system0_broker: System0ToolBroker) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(task) = system0_broker.wait_for_next_task().await else {
                break;
            };

            if system0_broker
                .is_agent_circuit_open(&task.assigned_agent)
                .await
            {
                let expected_from = task_state_label(&task.state);
                if let Err(err) = system0_broker
                    .transition_task(
                        task.id,
                        Some(expected_from),
                        TaskState::Failed {
                            error: task_failure_payload(format!(
                                "dispatch blocked by open circuit breaker for '{}'",
                                task.assigned_agent
                            )),
                        },
                        None,
                    )
                    .await
                {
                    tracing::error!(
                        task_id = %task.id,
                        agent_id = %task.assigned_agent,
                        error = ?err,
                        "task_failed_to_mark_circuit_open"
                    );
                }
                continue;
            }

            match &task.state {
                neuromancer_core::task::TaskState::Queued => {
                    let transition = system0_broker
                        .transition_task(
                            task.id,
                            Some("queued"),
                            neuromancer_core::task::TaskState::Dispatched,
                            None,
                        )
                        .await;
                    if let Err(err) = transition {
                        tracing::error!(task_id = %task.id, error = ?err, "task_transition_failed");
                        continue;
                    }

                    let transition = system0_broker
                        .transition_task(
                            task.id,
                            Some("dispatched"),
                            running_state(
                                neuromancer_core::agent::TaskExecutionState::Initializing {
                                    task_id: task.id,
                                },
                            ),
                            None,
                        )
                        .await;
                    if let Err(err) = transition {
                        tracing::error!(task_id = %task.id, error = ?err, "task_transition_failed");
                        continue;
                    }
                }
                neuromancer_core::task::TaskState::Dispatched => {
                    let transition = system0_broker
                        .transition_task(
                            task.id,
                            Some("dispatched"),
                            running_state(
                                neuromancer_core::agent::TaskExecutionState::Initializing {
                                    task_id: task.id,
                                },
                            ),
                            None,
                        )
                        .await;
                    if let Err(err) = transition {
                        tracing::error!(task_id = %task.id, error = ?err, "task_transition_failed");
                        continue;
                    }
                }
                neuromancer_core::task::TaskState::Running { .. } => {}
                _ => continue,
            }

            if let Err(err) = crate::orchestrator::actions::runtime_actions::execute_queued_task(
                system0_broker.clone(),
                task,
            )
            .await
            {
                tracing::error!(error = ?err, "task_dispatch_failed");
            }
        }
    })
}

pub(super) fn spawn_watchdog_worker(
    system0_broker: System0ToolBroker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut stale_since: HashMap<uuid::Uuid, Instant> = HashMap::new();

        loop {
            ticker.tick().await;
            let stale = system0_broker.stale_tasks_for_watchdog().await;
            let stale_ids = stale
                .iter()
                .map(|candidate| candidate.task_id)
                .collect::<std::collections::HashSet<_>>();
            stale_since.retain(|task_id, _| stale_ids.contains(task_id));

            for candidate in stale {
                use std::collections::hash_map::Entry;

                match stale_since.entry(candidate.task_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(Instant::now());
                        if let Err(err) = system0_broker
                            .cancel_task(
                                candidate.task_id,
                                format!(
                                    "watchdog cancellation: no heartbeat from '{}' for {:?}",
                                    candidate.agent_id, candidate.elapsed
                                ),
                            )
                            .await
                        {
                            tracing::error!(
                                task_id = %candidate.task_id,
                                agent_id = %candidate.agent_id,
                                error = ?err,
                                "watchdog_cancel_failed"
                            );
                        }
                    }
                    Entry::Occupied(entry) => {
                        if entry.get().elapsed() < candidate.watchdog_timeout {
                            continue;
                        }

                        let should_fail = match system0_broker.get_task(candidate.task_id).await {
                            Ok(Some(task)) => !task.state.is_terminal(),
                            Ok(None) => false,
                            Err(err) => {
                                tracing::error!(
                                    task_id = %candidate.task_id,
                                    agent_id = %candidate.agent_id,
                                    error = ?err,
                                    "watchdog_load_task_failed"
                                );
                                false
                            }
                        };
                        if should_fail {
                            let err = AgentError::Timeout {
                                task_id: candidate.task_id,
                                elapsed: entry.get().elapsed(),
                            };
                            if let Err(transition_err) = system0_broker
                                .transition_task(
                                    candidate.task_id,
                                    Some("running"),
                                    TaskState::Failed {
                                        error: task_failure_payload(err.to_string()),
                                    },
                                    None,
                                )
                                .await
                            {
                                tracing::error!(
                                    task_id = %candidate.task_id,
                                    agent_id = %candidate.agent_id,
                                    error = ?transition_err,
                                    "watchdog_force_fail_transition_failed"
                                );
                            }
                        }
                        stale_since.remove(&candidate.task_id);
                    }
                }
            }
        }
    })
}

fn task_state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Dispatched => "dispatched",
        TaskState::Running { .. } => "running",
        TaskState::Completed { .. } => "completed",
        TaskState::Failed { .. } => "failed",
        TaskState::Cancelled { .. } => "cancelled",
    }
}
