use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::InMemorySessionStore;
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::tool::ToolBroker;
use neuromancer_core::xdg::{XdgLayout, resolve_path};
use neuromancer_skills::SkillRegistry;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::orchestrator::bootstrap::{build_system0_agent_config, map_xdg_err};
use crate::orchestrator::error::System0Error;
use crate::orchestrator::llm_clients::{build_llm_client, resolve_tool_call_retry_limit};
use crate::orchestrator::prompt::{load_system_prompt_file, render_system0_prompt};
use crate::orchestrator::security::execution_guard::{ExecutionGuard, PlaceholderExecutionGuard};
use crate::orchestrator::skills::SkillToolBroker;
use crate::orchestrator::state::{SYSTEM0_AGENT_ID, System0ToolBroker};
use crate::orchestrator::tracing::thread_journal::{
    ThreadJournal, make_event, subagent_report_task_id, subagent_report_type,
};

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

pub(super) fn spawn_report_worker(
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

pub(super) fn build_subagents(
    config: &NeuromancerConfig,
    layout: &XdgLayout,
    config_dir: &Path,
    skill_registry: &SkillRegistry,
    local_root: &Path,
    execution_guard: &Arc<dyn ExecutionGuard>,
    report_tx: &mpsc::Sender<neuromancer_core::agent::SubAgentReport>,
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
        let broker: Arc<dyn ToolBroker> = Arc::new(
            SkillToolBroker::new(
                agent_id,
                &agent_config.capabilities.skills,
                skill_registry,
                local_root.to_path_buf(),
                execution_guard.clone(),
            )
            .map_err(|e| System0Error::Config(e.to_string()))?,
        );

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
