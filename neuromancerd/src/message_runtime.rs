use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use neuromancer_agent::llm::{LlmClient, LlmResponse, RigLlmClient};
use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_core::agent::{AgentConfig, SubAgentReport};
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::MessageSendResult;
use neuromancer_core::task::{Artifact, TaskState};
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult, ToolSource, ToolSpec,
};
use neuromancer_core::trigger::{Principal, TriggerEvent, TriggerMetadata, TriggerPayload};
use neuromancer_orchestrator::orchestrator::Orchestrator;
use neuromancer_orchestrator::registry::AgentRegistry;
use neuromancer_orchestrator::remediation::RemediationPolicy;
use neuromancer_orchestrator::router::Router;
use neuromancer_skills::{SkillMetadata, SkillRegistry};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum MessageRuntimeError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("runtime unavailable: {0}")]
    Unavailable(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("routing error: {0}")]
    Routing(String),

    #[error("dispatch error: {0}")]
    Dispatch(String),

    #[error("execution timed out after {0}")]
    Timeout(String),

    #[error("task failed: {0}")]
    TaskFailed(String),

    #[error("task requested user input: {0}")]
    InputRequired(String),

    #[error("task denied by policy: {0}")]
    PolicyDenied(String),

    #[error("task got stuck: {0}")]
    Stuck(String),

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
    inner: AsyncMutex<RuntimeInner>,
}

struct RuntimeInner {
    orchestrator: Orchestrator,
    report_rx: mpsc::Receiver<SubAgentReport>,
    required_skills_by_agent: HashMap<String, Vec<String>>,
    usage: Arc<Mutex<HashMap<uuid::Uuid, HashMap<String, u32>>>>,
    _workers: Vec<tokio::task::JoinHandle<()>>,
}

impl RuntimeInner {
    async fn wait_for_task(
        &mut self,
        task_id: uuid::Uuid,
        timeout: Duration,
    ) -> Result<(String, String), MessageRuntimeError> {
        wait_for_terminal_report(
            &mut self.orchestrator,
            &mut self.report_rx,
            task_id,
            timeout,
        )
        .await
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

        let router = Router::new(&config.routing);
        let mut registry = AgentRegistry::new();
        let (report_tx, report_rx) = mpsc::channel(64);
        let usage = Arc::new(Mutex::new(
            HashMap::<uuid::Uuid, HashMap<String, u32>>::new(),
        ));
        let mut required_skills_by_agent = HashMap::new();
        let mut workers = Vec::new();

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
                usage.clone(),
            )?);

            let (task_tx, mut task_rx) = mpsc::channel(16);
            registry.register(agent_id.clone(), agent_config.clone());
            registry.set_task_sender(agent_id, task_tx);
            required_skills_by_agent
                .insert(agent_id.clone(), agent_config.capabilities.skills.clone());

            let runtime = Arc::new(AgentRuntime::new(
                agent_config,
                llm_client,
                broker,
                report_tx.clone(),
            ));
            let worker_report_tx = report_tx.clone();

            let worker = tokio::spawn(async move {
                while let Some(mut task) = task_rx.recv().await {
                    if let Err(err) = runtime.execute(&mut task).await {
                        let _ = worker_report_tx
                            .send(SubAgentReport::Failed {
                                task_id: task.id,
                                error: err.to_string(),
                                partial_result: None,
                            })
                            .await;
                    }
                }
            });
            workers.push(worker);
        }

        let orchestrator = Orchestrator::new(router, registry, RemediationPolicy::default());

        Ok(Self {
            inner: AsyncMutex::new(RuntimeInner {
                orchestrator,
                report_rx,
                required_skills_by_agent,
                usage,
                _workers: workers,
            }),
        })
    }

    pub async fn send_message(
        &self,
        message: String,
    ) -> Result<MessageSendResult, MessageRuntimeError> {
        if message.trim().is_empty() {
            return Err(MessageRuntimeError::InvalidRequest(
                "message must not be empty".to_string(),
            ));
        }

        let mut inner = self.inner.lock().await;
        if inner.orchestrator.registry.is_empty() {
            return Err(MessageRuntimeError::Unavailable(
                "no agents are registered in runtime".to_string(),
            ));
        }

        let event = TriggerEvent {
            trigger_id: uuid::Uuid::new_v4().to_string(),
            occurred_at: Utc::now(),
            principal: Principal::Admin,
            payload: TriggerPayload::AdminCommand {
                instruction: message,
            },
            route_hint: None,
            metadata: TriggerMetadata::default(),
        };

        let task_id = inner
            .orchestrator
            .route(event)
            .await
            .map_err(|err| MessageRuntimeError::Routing(err.to_string()))?;

        let mut assigned_agent: Option<String> = None;
        while let Some(task) = inner.orchestrator.task_queue.dequeue() {
            let agent_id = task.assigned_agent.clone();
            let dispatch_task_id = task.id;
            if dispatch_task_id == task_id {
                assigned_agent = Some(agent_id.clone());
            }

            inner
                .orchestrator
                .registry
                .dispatch(&agent_id, task)
                .await
                .map_err(|err| MessageRuntimeError::Dispatch(err.to_string()))?;
            inner
                .orchestrator
                .task_queue
                .update_state(dispatch_task_id, TaskState::Dispatched);
            inner
                .orchestrator
                .registry
                .set_current_task(&agent_id, Some(dispatch_task_id));
        }

        let assigned_agent = assigned_agent.ok_or_else(|| {
            MessageRuntimeError::Internal(
                "orchestrator did not dispatch a task for the submitted message".to_string(),
            )
        })?;

        let (summary, response) = inner.wait_for_task(task_id, MESSAGE_TIMEOUT).await?;

        let usage = {
            let mut usage_guard = inner
                .usage
                .lock()
                .map_err(|_| MessageRuntimeError::Internal("usage lock poisoned".to_string()))?;
            usage_guard.remove(&task_id).unwrap_or_default()
        };

        if let Some(required_skills) = inner.required_skills_by_agent.get(&assigned_agent) {
            for skill in required_skills {
                if usage.get(skill).copied().unwrap_or(0) == 0 {
                    return Err(MessageRuntimeError::TaskFailed(format!(
                        "required skill '{}' was not used while handling the message",
                        skill
                    )));
                }
            }
        }

        Ok(MessageSendResult {
            task_id: task_id.to_string(),
            assigned_agent,
            state: "completed".to_string(),
            summary,
            response,
            tool_usage: usage,
        })
    }
}

fn append_tool_requirements(preamble: Option<String>, skills: &[String]) -> String {
    let mut text = preamble.unwrap_or_else(|| "You are a helpful sub-agent.".to_string());
    if !skills.is_empty() {
        let joined = skills.join(", ");
        text.push_str(
            "\n\nYou must use the available skill tools to gather required data before answering.",
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
    issued_tools: Mutex<bool>,
}

#[async_trait::async_trait]
impl LlmClient for TwoStepMockLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        _messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<LlmResponse, NeuromancerError> {
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
                    arguments: serde_json::json!({}),
                })
                .collect();

            return Ok(LlmResponse {
                text: None,
                tool_calls: calls,
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        *issued_tools = false;
        Ok(LlmResponse {
            text: Some("Based on your bills and account balances, prioritize the earliest due high-amount bill first. Current cash appears sufficient.".to_string()),
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
    ) -> Result<LlmResponse, NeuromancerError> {
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

        Ok(LlmResponse {
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
    usage: Arc<Mutex<HashMap<uuid::Uuid, HashMap<String, u32>>>>,
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
        usage: Arc<Mutex<HashMap<uuid::Uuid, HashMap<String, u32>>>>,
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

        Ok(Self {
            tools,
            local_root,
            usage,
        })
    }

    fn bump_usage(
        usage: &Arc<Mutex<HashMap<uuid::Uuid, HashMap<String, u32>>>>,
        task_id: uuid::Uuid,
        tool_name: &str,
    ) -> Result<(), MessageRuntimeError> {
        let mut guard = usage
            .lock()
            .map_err(|_| MessageRuntimeError::Internal("usage lock poisoned".to_string()))?;
        let entry = guard.entry(task_id).or_default();
        let count = entry.entry(tool_name.to_string()).or_insert(0);
        *count += 1;
        Ok(())
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

        if let Err(err) = Self::bump_usage(&self.usage, ctx.task_id, &call.tool_id) {
            return Err(NeuromancerError::Tool(ToolError::ExecutionFailed {
                tool_id: call.tool_id.clone(),
                message: err.to_string(),
            }));
        }

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

async fn wait_for_terminal_report(
    orchestrator: &mut Orchestrator,
    report_rx: &mut mpsc::Receiver<SubAgentReport>,
    task_id: uuid::Uuid,
    timeout: Duration,
) -> Result<(String, String), MessageRuntimeError> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(MessageRuntimeError::Timeout(
                humantime::format_duration(timeout).to_string(),
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let report = tokio::time::timeout(remaining, report_rx.recv())
            .await
            .map_err(|_| {
                MessageRuntimeError::Timeout(humantime::format_duration(timeout).to_string())
            })?
            .ok_or_else(|| {
                MessageRuntimeError::Internal(
                    "report channel closed before task completion".to_string(),
                )
            })?;

        let report_task_id = report_task_id(&report);
        orchestrator.handle_report(report.clone()).await;
        if report_task_id != task_id {
            continue;
        }

        match report {
            SubAgentReport::Completed {
                summary, artifacts, ..
            } => {
                let response = extract_response_text(&artifacts).unwrap_or_else(|| summary.clone());
                return Ok((summary, response));
            }
            SubAgentReport::Failed { error, .. } => {
                return Err(MessageRuntimeError::TaskFailed(error));
            }
            SubAgentReport::InputRequired { question, .. } => {
                return Err(MessageRuntimeError::InputRequired(question));
            }
            SubAgentReport::PolicyDenied {
                action,
                capability_needed,
                ..
            } => {
                return Err(MessageRuntimeError::PolicyDenied(format!(
                    "action '{}' requires capability '{}'",
                    action, capability_needed
                )));
            }
            SubAgentReport::Stuck { reason, .. } => {
                return Err(MessageRuntimeError::Stuck(reason));
            }
            SubAgentReport::Progress { .. } | SubAgentReport::ToolFailure { .. } => {}
        }
    }
}

fn report_task_id(report: &SubAgentReport) -> uuid::Uuid {
    match report {
        SubAgentReport::Progress { task_id, .. }
        | SubAgentReport::InputRequired { task_id, .. }
        | SubAgentReport::ToolFailure { task_id, .. }
        | SubAgentReport::PolicyDenied { task_id, .. }
        | SubAgentReport::Stuck { task_id, .. }
        | SubAgentReport::Completed { task_id, .. }
        | SubAgentReport::Failed { task_id, .. } => *task_id,
    }
}

fn extract_response_text(artifacts: &[Artifact]) -> Option<String> {
    artifacts.first().map(|artifact| artifact.content.clone())
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

    #[cfg(unix)]
    #[test]
    fn resolve_local_data_path_rejects_symlink_escape() {
        let local_root = temp_dir("nm_local_root");
        let outside_root = temp_dir("nm_outside_root");
        let outside_file = outside_root.join("secrets.txt");
        fs::write(&outside_file, "secret").expect("write outside");

        let data_dir = local_root.join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        let link_path = data_dir.join("escape.txt");
        std::os::unix::fs::symlink(&outside_file, &link_path).expect("symlink");

        let err = resolve_local_data_path(&local_root, "data/escape.txt")
            .expect_err("symlink escape should be rejected");
        assert!(matches!(err, MessageRuntimeError::PathViolation(_)));
    }
}
