use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use neuromancer_agent::llm::{LlmClient, RigLlmClient};
use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::agent::{AgentConfig, AgentHealthConfig, AgentMode, AgentModelConfig};
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{DelegatedRun, OrchestratorTurnResult};
use neuromancer_core::task::Task;
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult, ToolSource, ToolSpec,
};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_skills::{SkillMetadata, SkillRegistry};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

const TURN_TIMEOUT: Duration = Duration::from_secs(180);

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
}

impl RuntimeCore {
    async fn process_turn(
        &mut self,
        message: String,
        trigger_type: TriggerType,
    ) -> Result<OrchestratorTurnResult, MessageRuntimeError> {
        let turn_id = uuid::Uuid::new_v4();
        self.system0_broker
            .set_turn_context(turn_id, trigger_type)
            .await;

        let output = self
            .orchestrator_runtime
            .execute_turn(
                &self.session_store,
                self.session_id,
                TriggerSource::Cli,
                message,
            )
            .await
            .map_err(|err| MessageRuntimeError::Internal(err.to_string()))?;

        let response =
            extract_response_text(&output.output).unwrap_or_else(|| output.output.summary.clone());
        let delegated_runs = self.system0_broker.take_runs(turn_id).await;

        Ok(OrchestratorTurnResult {
            turn_id: turn_id.to_string(),
            response,
            delegated_runs,
        })
    }
}

impl MessageRuntime {
    pub async fn new(config: &NeuromancerConfig) -> Result<Self, MessageRuntimeError> {
        let (skills_dir, local_root) = default_xdg_paths()?;

        let mut skill_registry = SkillRegistry::new(vec![skills_dir.clone()]);
        skill_registry
            .scan()
            .await
            .map_err(|err| MessageRuntimeError::Config(err.to_string()))?;

        let (report_tx, mut report_rx) = mpsc::channel(256);
        let report_worker = tokio::spawn(async move { while report_rx.recv().await.is_some() {} });

        let mut subagents = HashMap::<String, Arc<AgentRuntime>>::new();
        for (agent_id, agent_toml) in &config.agents {
            let mut agent_config = agent_toml.to_agent_config(agent_id);
            agent_config.preamble = Some(append_tool_requirements(
                agent_config.preamble,
                &agent_config.capabilities.skills,
            ));

            let llm_client = build_llm_client(config, &agent_config)?;
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
            ));
            subagents.insert(agent_id.clone(), runtime);
        }

        if subagents.is_empty() {
            return Err(MessageRuntimeError::Config(
                "no sub-agents are configured in runtime".to_string(),
            ));
        }

        let config_snapshot = serde_json::to_value(config)
            .map_err(|err| MessageRuntimeError::Config(err.to_string()))?;

        let system0_broker = System0ToolBroker::new(
            subagents,
            config_snapshot,
            &config.orchestrator.capabilities.skills,
        );
        let runtime_broker = system0_broker.clone();

        let orchestrator_config = build_orchestrator_config(config);
        let orchestrator_llm = build_llm_client(config, &orchestrator_config)?;
        let orchestrator_runtime = Arc::new(AgentRuntime::new(
            orchestrator_config,
            orchestrator_llm,
            Arc::new(system0_broker.clone()),
            report_tx,
        ));

        let core = Arc::new(AsyncMutex::new(RuntimeCore {
            orchestrator_runtime,
            session_store: InMemorySessionStore::new(),
            session_id: uuid::Uuid::new_v4(),
            system0_broker,
        }));

        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnRequest>(128);
        let worker_core = core.clone();
        let turn_worker = tokio::spawn(async move {
            while let Some(request) = turn_rx.recv().await {
                let result = {
                    let mut core = worker_core.lock().await;
                    core.process_turn(request.message, request.trigger_type)
                        .await
                };
                let _ = request.response_tx.send(result);
            }
        });

        Ok(Self {
            turn_tx,
            system0_broker: runtime_broker,
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
}

fn build_orchestrator_config(config: &NeuromancerConfig) -> AgentConfig {
    let mut capabilities = config.orchestrator.capabilities.clone();
    if capabilities.skills.is_empty() {
        capabilities.skills = vec![
            "delegate_to_agent".to_string(),
            "list_agents".to_string(),
            "read_config".to_string(),
            "modify_skill".to_string(),
        ];
    }

    AgentConfig {
        id: "system0".to_string(),
        mode: AgentMode::Inproc,
        image: None,
        models: AgentModelConfig {
            planner: None,
            executor: config.orchestrator.model_slot.clone(),
            verifier: None,
        },
        capabilities,
        health: AgentHealthConfig::default(),
        preamble: Some(config.orchestrator.preamble.clone().unwrap_or_else(|| {
            "You are System 0. You mediate user intent and delegate to specialized sub-agents using tools.".to_string()
        })),
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
    current_trigger_type: TriggerType,
    current_turn_id: uuid::Uuid,
    runs_by_turn: HashMap<uuid::Uuid, Vec<DelegatedRun>>,
    runs_index: HashMap<String, DelegatedRun>,
    runs_order: Vec<String>,
    running_agents: HashMap<String, String>,
}

impl System0ToolBroker {
    fn new(
        subagents: HashMap<String, Arc<AgentRuntime>>,
        config_snapshot: serde_json::Value,
        allowlisted_tools: &[String],
    ) -> Self {
        let default_tools = vec![
            "delegate_to_agent".to_string(),
            "list_agents".to_string(),
            "read_config".to_string(),
            "modify_skill".to_string(),
        ];

        let allowlisted_tools = if allowlisted_tools.is_empty() {
            default_tools.into_iter().collect()
        } else {
            allowlisted_tools.iter().cloned().collect()
        };

        Self {
            inner: Arc::new(AsyncMutex::new(System0BrokerInner {
                subagents,
                config_snapshot,
                allowlisted_tools,
                current_trigger_type: TriggerType::Admin,
                current_turn_id: uuid::Uuid::nil(),
                runs_by_turn: HashMap::new(),
                runs_index: HashMap::new(),
                runs_order: Vec::new(),
                running_agents: HashMap::new(),
            })),
        }
    }

    async fn set_turn_context(&self, turn_id: uuid::Uuid, trigger_type: TriggerType) {
        let mut inner = self.inner.lock().await;
        inner.current_turn_id = turn_id;
        inner.current_trigger_type = trigger_type;
        inner.runs_by_turn.remove(&turn_id);
    }

    async fn take_runs(&self, turn_id: uuid::Uuid) -> Vec<DelegatedRun> {
        let mut inner = self.inner.lock().await;
        inner.runs_by_turn.remove(&turn_id).unwrap_or_default()
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

        if !inner.allowlisted_tools.contains(&call.tool_id) {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: call.tool_id,
            }));
        }

        match call.tool_id.as_str() {
            "list_agents" => Ok(ToolResult {
                call_id: call.id,
                output: ToolOutput::Success(serde_json::json!({
                    "agents": inner.subagents.keys().cloned().collect::<Vec<_>>()
                })),
            }),
            "read_config" => Ok(ToolResult {
                call_id: call.id,
                output: ToolOutput::Success(inner.config_snapshot.clone()),
            }),
            "modify_skill" => {
                if inner.current_trigger_type != TriggerType::Admin {
                    return Ok(ToolResult {
                        call_id: call.id,
                        output: ToolOutput::Error(
                            "modify_skill requires an admin trigger".to_string(),
                        ),
                    });
                }

                let skill_id = call
                    .arguments
                    .get("skill_id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        NeuromancerError::Tool(ToolError::ExecutionFailed {
                            tool_id: "modify_skill".to_string(),
                            message: "missing 'skill_id'".to_string(),
                        })
                    })?;

                Ok(ToolResult {
                    call_id: call.id,
                    output: ToolOutput::Success(serde_json::json!({
                        "status": "accepted",
                        "skill_id": skill_id,
                    })),
                })
            }
            "delegate_to_agent" => {
                let agent_id = call
                    .arguments
                    .get("agent_id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        NeuromancerError::Tool(ToolError::ExecutionFailed {
                            tool_id: "delegate_to_agent".to_string(),
                            message: "missing 'agent_id'".to_string(),
                        })
                    })?
                    .to_string();
                let instruction = call
                    .arguments
                    .get("instruction")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        NeuromancerError::Tool(ToolError::ExecutionFailed {
                            tool_id: "delegate_to_agent".to_string(),
                            message: "missing 'instruction'".to_string(),
                        })
                    })?
                    .to_string();

                let Some(runtime) = inner.subagents.get(&agent_id).cloned() else {
                    return Err(NeuromancerError::Tool(ToolError::NotFound {
                        tool_id: format!("delegate_to_agent:{agent_id}"),
                    }));
                };

                let run_id = uuid::Uuid::new_v4().to_string();
                let turn_id = inner.current_turn_id;
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
                    },
                );
                inner.runs_order.push(run_id.clone());
                drop(inner);

                let mut task = Task::new(TriggerSource::Internal, instruction, agent_id.clone());
                let result = runtime.execute(&mut task).await;

                let mut inner = self.inner.lock().await;
                inner.running_agents.remove(&agent_id);

                let run = match result {
                    Ok(output) => DelegatedRun {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        state: "completed".to_string(),
                        summary: Some(output.summary.clone()),
                    },
                    Err(err) => DelegatedRun {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        state: "failed".to_string(),
                        summary: Some(err.to_string()),
                    },
                };

                inner
                    .runs_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(run.clone());
                inner.runs_index.insert(run.run_id.clone(), run.clone());

                Ok(ToolResult {
                    call_id: call.id,
                    output: ToolOutput::Success(serde_json::json!({
                        "run_id": run.run_id,
                        "agent_id": run.agent_id,
                        "state": run.state,
                        "summary": run.summary,
                    })),
                })
            }
            _ => Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: call.tool_id,
            })),
        }
    }
}

fn append_tool_requirements(preamble: Option<String>, skills: &[String]) -> String {
    let mut text = preamble.unwrap_or_else(|| "You are a helpful sub-agent.".to_string());
    if !skills.is_empty() {
        let joined = skills.join(", ");
        text.push_str(
            "\n\nYou can use allowlisted skill tools to gather required data before answering.",
        );
        text.push_str(&format!("\nAvailable tools: {joined}."));
    }
    text
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
            let groq = rig::providers::groq::Client::new(&key);
            Ok(Arc::new(RigLlmClient::new(
                groq.completion_model(&slot.model),
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
        _messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<neuromancer_agent::llm::LlmResponse, NeuromancerError> {
        let mut issued_tools = self.issued_tools.lock().map_err(|_| {
            NeuromancerError::Infra(neuromancer_core::error::InfraError::Config(
                "mock llm lock poisoned".to_string(),
            ))
        })?;

        if !*issued_tools && !tool_definitions.is_empty() {
            *issued_tools = true;
            let calls = tool_definitions
                .iter()
                .enumerate()
                .map(|(idx, tool)| ToolCall {
                    id: format!("mock-call-{}", idx + 1),
                    tool_id: tool.name.clone(),
                    arguments: match tool.name.as_str() {
                        "delegate_to_agent" => serde_json::json!({
                            "agent_id": "planner",
                            "instruction": "Summarize the user request"
                        }),
                        "modify_skill" => serde_json::json!({
                            "skill_id": "demo",
                            "patch": "no-op"
                        }),
                        _ => serde_json::json!({}),
                    },
                })
                .collect();

            return Ok(neuromancer_agent::llm::LlmResponse {
                text: None,
                tool_calls: calls,
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        *issued_tools = false;
        Ok(neuromancer_agent::llm::LlmResponse {
            text: Some("System0 turn completed.".to_string()),
            tool_calls: vec![],
            prompt_tokens: 0,
            completion_tokens: 0,
        })
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
    local_root: PathBuf,
}

#[derive(Clone)]
struct SkillTool {
    description: String,
    markdown_paths: Vec<String>,
    csv_paths: Vec<String>,
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
            let metadata = skill_registry.get_metadata(skill_name).ok_or_else(|| {
                MessageRuntimeError::Config(format!(
                    "agent '{}' references missing skill '{}'",
                    agent_id, skill_name
                ))
            })?;

            tools.insert(skill_name.clone(), skill_tool_from_metadata(metadata));
        }

        Ok(Self { tools, local_root })
    }
}

#[async_trait::async_trait]
impl ToolBroker for SkillToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        ctx.allowed_tools
            .iter()
            .filter_map(|name| {
                self.tools.get(name).map(|tool| ToolSpec {
                    id: name.clone(),
                    name: name.clone(),
                    description: tool.description.clone(),
                    parameters_schema: serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    source: ToolSource::Skill {
                        skill_id: name.clone(),
                    },
                })
            })
            .collect()
    }

    async fn call_tool(
        &self,
        ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        if !ctx.allowed_tools.iter().any(|tool| tool == &call.tool_id) {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: call.tool_id,
            }));
        }

        let Some(tool) = self.tools.get(&call.tool_id) else {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: call.tool_id,
            }));
        };

        let mut markdown_docs = Vec::new();
        for raw in &tool.markdown_paths {
            let path = resolve_local_data_path(&self.local_root, raw)
                .map_err(map_tool_err(&call.tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: call.tool_id.clone(),
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
                .map_err(map_tool_err(&call.tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: call.tool_id.clone(),
                    message: err.to_string(),
                })
            })?;
            let parsed = parse_csv_content(&path, &content).map_err(map_tool_err(&call.tool_id))?;
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

        Ok(ToolResult {
            call_id: call.id,
            output: ToolOutput::Success(serde_json::json!({
                "skill": call.tool_id,
                "markdown": markdown_docs,
                "csv": csv_docs,
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

fn skill_tool_from_metadata(metadata: &SkillMetadata) -> SkillTool {
    SkillTool {
        description: if metadata.description.trim().is_empty() {
            format!("Skill {}", metadata.name)
        } else {
            metadata.description.clone()
        },
        markdown_paths: metadata.data_sources.markdown.clone(),
        csv_paths: metadata.data_sources.csv.clone(),
    }
}

fn default_xdg_paths() -> Result<(PathBuf, PathBuf), MessageRuntimeError> {
    let home = home_dir()?;
    let skills_dir = home.join(".config/neuromancer/skills");
    let local_root = home.join(".local/neuromancer");
    Ok((skills_dir, local_root))
}

fn home_dir() -> Result<PathBuf, MessageRuntimeError> {
    let home = std::env::var("HOME").map_err(|_| {
        MessageRuntimeError::Config("HOME environment variable is not set".to_string())
    })?;
    Ok(PathBuf::from(home))
}

fn resolve_local_data_path(
    local_root: &Path,
    relative: &str,
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

    let root_canonical = fs::canonicalize(local_root).map_err(|err| {
        MessageRuntimeError::ResourceNotFound(format!(
            "local data root '{}' is unavailable: {err}",
            local_root.display()
        ))
    })?;

    let full_path = local_root.join(input);
    let target_canonical = fs::canonicalize(&full_path).map_err(|err| {
        MessageRuntimeError::ResourceNotFound(format!(
            "data file '{}' is unavailable: {err}",
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

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
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
}
