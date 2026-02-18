//! `AgentRuntime` state machine: LLM completions + tool execution until final response.

mod helpers;
mod recovery;

use std::sync::Arc;
use std::time::Instant;

use neuromancer_core::agent::{AgentConfig, SubAgentReport, TaskExecutionState};
use neuromancer_core::argument_tokens;
use neuromancer_core::error::{AgentError, NeuromancerError};
use neuromancer_core::secrets::{SecretRef, SecretUsage, SecretsBroker};
use neuromancer_core::task::{
    AgentErrorLike, AgentOutput, Artifact, ArtifactKind, Checkpoint, Task, TaskId, TaskOutput,
    TaskState, TokenUsage,
};
use neuromancer_core::thread::{ThreadId, ThreadStore};
use neuromancer_core::tool::{AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::TriggerSource;

use crate::conversation::{ChatMessage, ConversationContext, TruncationStrategy};
use crate::llm::LlmClient;

use helpers::{available_tool_names, specs_to_rig_definitions, truncate_summary};

#[derive(Debug, Clone)]
pub struct TurnExecutionResult {
    pub task_id: uuid::Uuid,
    pub output: AgentOutput,
}

/// Agent runtime that manages a single task execution through the state machine.
pub struct AgentRuntime {
    config: AgentConfig,
    llm_client: Arc<dyn LlmClient>,
    tool_broker: Arc<dyn ToolBroker>,
    secrets_broker: Option<Arc<dyn SecretsBroker>>,
    report_tx: tokio::sync::mpsc::Sender<SubAgentReport>,
    tool_call_retry_limit: u32,
}

impl AgentRuntime {
    pub fn new(
        config: AgentConfig,
        llm_client: Arc<dyn LlmClient>,
        tool_broker: Arc<dyn ToolBroker>,
        report_tx: tokio::sync::mpsc::Sender<SubAgentReport>,
        tool_call_retry_limit: u32,
    ) -> Self {
        Self {
            config,
            llm_client,
            tool_broker,
            secrets_broker: None,
            report_tx,
            tool_call_retry_limit,
        }
    }

    pub fn with_secrets_broker(mut self, broker: Arc<dyn SecretsBroker>) -> Self {
        self.secrets_broker = Some(broker);
        self
    }

    // TODO: there seems to be significant code duplication between `execute` and `execute_turn`

    /// Execute a task through the full state machine lifecycle.
    /// Returns the final TaskOutput on success.
    pub async fn execute(&self, task: &mut Task) -> Result<TaskOutput, NeuromancerError> {
        let start = Instant::now();
        let mut total_usage = TokenUsage::default();

        // Transition: Initializing
        task.state = TaskState::Running {
            execution_state: TaskExecutionState::Initializing { task_id: task.id },
        };
        tracing::info!(
            task_id = %task.id,
            agent_id = %self.config.id,
            "agent runtime initializing for task"
        );

        // Build agent context for tool broker
        let agent_ctx = AgentContext {
            agent_id: self.config.id.clone(),
            task_id: task.id,
            allowed_tools: self.config.capabilities.skills.clone(),
            allowed_mcp_servers: self.config.capabilities.mcp_servers.clone(),
            allowed_peer_agents: self.config.capabilities.a2a_peers.clone(),
            allowed_secrets: self.config.capabilities.secrets.clone(),
            allowed_memory_partitions: self.config.capabilities.memory_partitions.clone(),
        };

        // Build initial conversation context
        // NOTE: [med pri] this is good. See if there are conventions which may be borrowed from OpenClaw
        let mut conversation = ConversationContext::new(
            128_000, // default token budget
            TruncationStrategy::SlidingWindow { keep_last: 50 },
        );

        // Add system prompt
        // NOTE: [med pri] additional Markdown stack (eg: SOUL.md, USER.md, etc) would be compiled here.
        // Ideally, these would be part of the config and included in `agent_ctx`
        let system_prompt = self.config.system_prompt.clone();
        conversation.add_message(ChatMessage::system(&system_prompt));

        // Add task instruction as user message
        conversation.add_message(ChatMessage::user(&task.instruction));

        // Gather available tool definitions
        let tool_specs = self.tool_broker.list_tools(&agent_ctx).await;
        let available_tool_names = available_tool_names(&tool_specs);
        // NOTE: [low pri] shouldn't this be done in advance?
        let tool_defs = specs_to_rig_definitions(&tool_specs);

        let max_iterations = self.config.max_iterations;
        let mut iteration: u32 = 0;
        let mut invalid_tool_call_recovery_attempts: u32 = 0;

        // Main Thinking -> Acting loop
        loop {
            iteration += 1;
            if iteration > max_iterations {
                let err = AgentError::MaxIterationsExceeded {
                    task_id: task.id,
                    iterations: iteration,
                };
                self.send_report(SubAgentReport::Stuck {
                    task_id: task.id,
                    thread_id: None,
                    reason: format!("max iterations ({max_iterations}) exceeded"),
                    partial_result: None,
                })
                .await;
                self.send_report(SubAgentReport::Failed {
                    task_id: task.id,
                    thread_id: None,
                    error: err.to_string(),
                    partial_result: None,
                })
                .await;
                task.state = TaskState::Failed {
                    error: AgentErrorLike::new("max_iterations_exceeded", err.to_string()),
                };
                return Err(NeuromancerError::Agent(err));
            }

            tracing::debug!(
                task_id = %task.id,
                iteration,
                "thinking: calling LLM"
            );

            // Thinking: call LLM
            // NOTE: [low pri] should this log something?
            conversation.maybe_truncate();
            let rig_messages = conversation.to_rig_messages();
            let response = match self
                .llm_client
                .complete(&system_prompt, rig_messages, tool_defs.clone())
                .await
            {
                Ok(response) => response,
                Err(err) => match self
                    .try_recover_invalid_tool_call(
                        &err,
                        &mut conversation,
                        task.id,
                        &available_tool_names,
                        &mut invalid_tool_call_recovery_attempts,
                    )
                    .await
                {
                    Ok(true) => continue,
                    Ok(false) => return Err(err),
                    Err(exhausted) => return Err(exhausted),
                },
            };

            total_usage.prompt_tokens += response.prompt_tokens;
            total_usage.completion_tokens += response.completion_tokens;
            total_usage.total_tokens += response.prompt_tokens + response.completion_tokens;

            // Send progress report
            if iteration % 5 == 0 {
                self.send_report(SubAgentReport::Progress {
                    task_id: task.id,
                    thread_id: None,
                    step: iteration,
                    description: format!("iteration {iteration}/{max_iterations}"),
                    artifacts_so_far: vec![],
                })
                .await;
            }

            if response.has_tool_calls() {
                // Acting: execute tool calls
                tracing::debug!(
                    task_id = %task.id,
                    num_calls = response.tool_calls.len(),
                    "acting: executing tool calls"
                );

                // Add assistant's tool call message
                conversation.add_message(ChatMessage::assistant_tool_calls(
                    response.tool_calls.clone(),
                ));

                for call in &response.tool_calls {
                    let result = self.execute_tool_call(&agent_ctx, call).await;

                    match &result.output {
                        ToolOutput::Error(err) => {
                            tracing::warn!(
                                task_id = %task.id,
                                tool_id = %call.tool_id,
                                error = %err,
                                "tool call failed"
                            );
                            self.send_report(SubAgentReport::ToolFailure {
                                task_id: task.id,
                                thread_id: None,
                                tool_id: call.tool_id.clone(),
                                error: err.clone(),
                                retry_eligible: true,
                                attempted_count: 1,
                            })
                            .await;
                        }
                        ToolOutput::Success(_) => {
                            tracing::debug!(
                                task_id = %task.id,
                                tool_id = %call.tool_id,
                                "tool call succeeded"
                            );
                        }
                    }

                    // Add tool result to conversation
                    conversation.add_message(ChatMessage::tool_result(result));
                }

                // Continue the loop (back to Thinking)
                continue;
            }

            // No tool calls: this is a final response
            // Transition to Completed
            let output_text = response.text.unwrap_or_default();

            tracing::info!(
                task_id = %task.id,
                iterations = iteration,
                "task completed"
            );

            let output = TaskOutput {
                artifacts: vec![Artifact {
                    kind: ArtifactKind::Text,
                    name: "response".into(),
                    content: output_text.clone(),
                    mime_type: Some("text/plain".into()),
                }],
                summary: truncate_summary(&output_text, 200),
                token_usage: total_usage,
                duration: start.elapsed(),
            };

            // Create checkpoint
            let checkpoint = Checkpoint {
                task_id: task.id,
                state_data: serde_json::to_value(&TaskExecutionState::Completed {
                    output: output.clone(),
                })
                .unwrap_or_default(),
                created_at: chrono::Utc::now(),
            };
            task.checkpoints.push(checkpoint);

            self.send_report(SubAgentReport::Completed {
                task_id: task.id,
                thread_id: None,
                artifacts: output.artifacts.clone(),
                summary: output.summary.clone(),
            })
            .await;
            task.state = TaskState::Completed {
                output: output.clone(),
            };

            return Ok(output);
        }
    }

    /// Execute a single conversational turn backed by a persistent ThreadStore.
    ///
    /// Loads existing conversation history from the thread, runs the thinking-acting loop,
    /// then flushes only the new messages (delta) back to the thread store.
    ///
    /// `injected_context` is an optional set of messages (e.g. memory summaries, task status)
    /// that are prepended to the conversation after the system prompt and persisted history.
    /// These are ephemeral context — they are NOT persisted to the thread store.
    pub async fn execute_turn_with_thread_store(
        &self,
        thread_store: &dyn ThreadStore,
        thread_id: &ThreadId,
        source: TriggerSource,
        user_message: String,
        task_id: TaskId,
        injected_context: Vec<ChatMessage>,
    ) -> Result<TurnExecutionResult, NeuromancerError> {
        let mut task = Task::new_with_id(
            task_id,
            source,
            user_message.clone(),
            self.config.id.clone(),
        );
        let start = Instant::now();
        let mut total_usage = TokenUsage::default();

        task.state = TaskState::Running {
            execution_state: TaskExecutionState::Initializing { task_id: task.id },
        };
        tracing::info!(
            task_id = %task.id,
            agent_id = %self.config.id,
            "agent runtime thread-backed turn execution starting"
        );

        let agent_ctx = AgentContext {
            agent_id: self.config.id.clone(),
            task_id: task.id,
            allowed_tools: self.config.capabilities.skills.clone(),
            allowed_mcp_servers: self.config.capabilities.mcp_servers.clone(),
            allowed_peer_agents: self.config.capabilities.a2a_peers.clone(),
            allowed_secrets: self.config.capabilities.secrets.clone(),
            allowed_memory_partitions: self.config.capabilities.memory_partitions.clone(),
        };

        // Load existing thread messages
        let existing_messages = thread_store.load_messages(thread_id, false).await?;

        // Build conversation context
        let system_prompt = self.config.system_prompt.clone();
        let mut conversation = ConversationContext::new(
            u32::MAX,
            TruncationStrategy::SlidingWindow { keep_last: 50 },
        );

        // System prompt first
        conversation.add_message(ChatMessage::system(&system_prompt));

        // Replay persisted history
        for msg in existing_messages {
            conversation.add_message(msg);
        }

        // Inject ephemeral context (memory summaries, task status, etc.)
        // These are NOT persisted — they sit between history and the new user message.
        for msg in injected_context {
            conversation.add_message(msg);
        }

        // Record where this turn's new messages begin (after history + injected context)
        let pre_turn_count = conversation.messages.len();

        // Add the new user message
        conversation.add_message(ChatMessage::user(user_message));

        // Gather available tool definitions
        let tool_specs = self.tool_broker.list_tools(&agent_ctx).await;
        let available_tool_names = available_tool_names(&tool_specs);
        let tool_defs = specs_to_rig_definitions(&tool_specs);
        let max_iterations = self.config.max_iterations;
        let mut iteration: u32 = 0;
        let mut invalid_tool_call_recovery_attempts: u32 = 0;

        let run_result = loop {
            iteration += 1;
            if iteration > max_iterations {
                let err = AgentError::MaxIterationsExceeded {
                    task_id: task.id,
                    iterations: iteration,
                };
                self.send_report(SubAgentReport::Stuck {
                    task_id: task.id,
                    thread_id: None,
                    reason: format!("max iterations ({max_iterations}) exceeded"),
                    partial_result: None,
                })
                .await;
                break Err(NeuromancerError::Agent(err));
            }

            tracing::debug!(
                task_id = %task.id,
                iteration,
                "thinking: calling LLM (thread-backed turn)"
            );

            let rig_messages = conversation.to_rig_messages();
            let response = match self
                .llm_client
                .complete(&system_prompt, rig_messages, tool_defs.clone())
                .await
            {
                Ok(response) => response,
                Err(err) => match self
                    .try_recover_invalid_tool_call(
                        &err,
                        &mut conversation,
                        task.id,
                        &available_tool_names,
                        &mut invalid_tool_call_recovery_attempts,
                    )
                    .await
                {
                    Ok(true) => continue,
                    Ok(false) => break Err(err),
                    Err(exhausted) => break Err(exhausted),
                },
            };

            total_usage.prompt_tokens += response.prompt_tokens;
            total_usage.completion_tokens += response.completion_tokens;
            total_usage.total_tokens += response.prompt_tokens + response.completion_tokens;

            if iteration % 5 == 0 {
                self.send_report(SubAgentReport::Progress {
                    task_id: task.id,
                    thread_id: None,
                    step: iteration,
                    description: format!("iteration {iteration}/{max_iterations}"),
                    artifacts_so_far: vec![],
                })
                .await;
            }

            if response.has_tool_calls() {
                conversation.add_message(ChatMessage::assistant_tool_calls(
                    response.tool_calls.clone(),
                ));

                for call in &response.tool_calls {
                    let result = self.execute_tool_call(&agent_ctx, call).await;

                    if let ToolOutput::Error(err) = &result.output {
                        self.send_report(SubAgentReport::ToolFailure {
                            task_id: task.id,
                            thread_id: None,
                            tool_id: call.tool_id.clone(),
                            error: err.clone(),
                            retry_eligible: true,
                            attempted_count: 1,
                        })
                        .await;
                    }

                    conversation.add_message(ChatMessage::tool_result(result));
                }

                continue;
            }

            let output_text = response.text.unwrap_or_default();
            conversation.add_message(ChatMessage::assistant_text(output_text.clone()));

            let agent_output = AgentOutput {
                message: output_text,
                token_usage: total_usage,
                duration: start.elapsed(),
            };

            // Convert to TaskOutput for state machine bookkeeping (checkpoint, task state).
            let task_output = agent_output.clone().into_task_output();

            let checkpoint = Checkpoint {
                task_id: task.id,
                state_data: serde_json::to_value(&TaskExecutionState::Completed {
                    output: task_output.clone(),
                })
                .unwrap_or_default(),
                created_at: chrono::Utc::now(),
            };
            task.checkpoints.push(checkpoint);

            self.send_report(SubAgentReport::Completed {
                task_id: task.id,
                thread_id: None,
                artifacts: task_output.artifacts.clone(),
                summary: task_output.summary.clone(),
            })
            .await;
            task.state = TaskState::Completed {
                output: task_output,
            };

            break Ok(agent_output);
        };

        // Flush new messages (delta) to thread store — always, even on failure
        let delta = &conversation.messages[pre_turn_count..];
        thread_store.append_messages(thread_id, delta).await?;

        match run_result {
            Ok(output) => Ok(TurnExecutionResult {
                task_id: task.id,
                output,
            }),
            Err(err) => {
                self.send_report(SubAgentReport::Failed {
                    task_id: task.id,
                    thread_id: None,
                    error: err.to_string(),
                    partial_result: None,
                })
                .await;
                task.state = TaskState::Failed {
                    error: AgentErrorLike::new("execution_failed", err.to_string()),
                };
                Err(err)
            }
        }
    }

    async fn execute_tool_call(&self, ctx: &AgentContext, call: &ToolCall) -> ToolResult {
        // Resolve secret handles in tool call arguments before dispatching.
        let resolved_call = match self.resolve_secret_handles(ctx, call).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Error(e.to_string()),
                };
            }
        };

        match self.tool_broker.call_tool(ctx, resolved_call).await {
            Ok(result) => result,
            Err(e) => ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Error(e.to_string()),
            },
        }
    }

    /// Scan tool call arguments for `{{HANDLE}}` tokens and resolve them via
    /// the SecretsBroker. Returns a new ToolCall with expanded arguments.
    /// If no SecretsBroker is configured or no handles are found, returns
    /// the original call unchanged.
    async fn resolve_secret_handles(
        &self,
        ctx: &AgentContext,
        call: &ToolCall,
    ) -> Result<ToolCall, NeuromancerError> {
        let broker = match &self.secrets_broker {
            Some(b) => b,
            None => return Ok(call.clone()),
        };

        let handle_refs = argument_tokens::extract_handle_refs(&call.arguments);
        if handle_refs.is_empty() {
            return Ok(call.clone());
        }

        let mut resolved = std::collections::HashMap::new();
        for handle in &handle_refs {
            if !ctx.allowed_secrets.contains(handle) {
                tracing::warn!(
                    agent_id = %ctx.agent_id,
                    handle = %handle,
                    tool_id = %call.tool_id,
                    "secret_handle_not_in_allowlist"
                );
                continue;
            }
            match broker
                .resolve_handle_for_tool(
                    ctx,
                    SecretRef::from(handle.as_str()),
                    SecretUsage {
                        tool_id: call.tool_id.clone(),
                        purpose: format!("tool_call:{}", call.id),
                    },
                )
                .await
            {
                Ok(secret) => {
                    resolved.insert(handle.clone(), secret.value);
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %ctx.agent_id,
                        handle = %handle,
                        error = ?e,
                        "secret_handle_resolution_failed"
                    );
                }
            }
        }

        if resolved.is_empty() {
            return Ok(call.clone());
        }

        Ok(ToolCall {
            id: call.id.clone(),
            tool_id: call.tool_id.clone(),
            arguments: argument_tokens::expand_secret_handles(&call.arguments, &resolved),
        })
    }

    async fn send_report(&self, report: SubAgentReport) {
        if let Err(e) = self.report_tx.send(report).await {
            tracing::error!("failed to send agent report: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmResponse, MockLlmClient};
    use neuromancer_core::agent::{
        AgentCapabilities, AgentHealthConfig, AgentMode, AgentModelConfig,
    };
    use neuromancer_core::error::LlmError;
    use neuromancer_core::task::Task;
    use neuromancer_core::tool::ToolSpec;
    use neuromancer_core::trigger::TriggerSource;

    struct MockToolBroker;

    #[async_trait::async_trait]
    impl ToolBroker for MockToolBroker {
        async fn list_tools(&self, _ctx: &AgentContext) -> Vec<ToolSpec> {
            vec![]
        }
        async fn call_tool(
            &self,
            _ctx: &AgentContext,
            call: ToolCall,
        ) -> Result<ToolResult, NeuromancerError> {
            Ok(ToolResult {
                call_id: call.id,
                output: ToolOutput::Success(serde_json::json!({"result": "ok"})),
            })
        }
    }

    enum SequenceItem {
        Response(LlmResponse),
        Error(NeuromancerError),
    }

    struct SequenceLlmClient {
        items: std::sync::Mutex<Vec<SequenceItem>>,
    }

    impl SequenceLlmClient {
        fn new(items: Vec<SequenceItem>) -> Self {
            Self {
                items: std::sync::Mutex::new(items),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for SequenceLlmClient {
        async fn complete(
            &self,
            _system_prompt: &str,
            _messages: Vec<rig::completion::Message>,
            _tool_definitions: Vec<rig::completion::ToolDefinition>,
        ) -> Result<LlmResponse, NeuromancerError> {
            let mut items = self.items.lock().expect("sequence lock");
            if items.is_empty() {
                return Ok(LlmResponse {
                    text: Some("sequence empty".into()),
                    tool_calls: vec![],
                    prompt_tokens: 0,
                    completion_tokens: 0,
                });
            }

            match items.remove(0) {
                SequenceItem::Response(response) => Ok(response),
                SequenceItem::Error(err) => Err(err),
            }
        }
    }

    fn test_config() -> AgentConfig {
        AgentConfig {
            id: "test-agent".into(),
            mode: AgentMode::Inproc,
            image: None,
            models: AgentModelConfig::default(),
            capabilities: AgentCapabilities::default(),
            health: AgentHealthConfig::default(),
            system_prompt: "You are a test agent.".into(),
            max_iterations: 5,
        }
    }

    fn invalid_tool_call_error(tool_name: &str) -> NeuromancerError {
        NeuromancerError::Llm(LlmError::InvalidResponse {
            reason: format!(
                "Tool call validation failed: attempted to call tool '{tool_name}' which was not in request.tools"
            ),
        })
    }

    #[tokio::test]
    async fn simple_text_response_completes() {
        let mock_llm = Arc::new(MockLlmClient::new(vec![LlmResponse {
            text: Some("Hello, task complete!".into()),
            tool_calls: vec![],
            prompt_tokens: 10,
            completion_tokens: 5,
        }]));
        let mock_broker = Arc::new(MockToolBroker);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let runtime = AgentRuntime::new(test_config(), mock_llm, mock_broker, tx, 1);
        let mut task = Task::new(
            TriggerSource::Internal,
            "Say hello".into(),
            "test-agent".into(),
        );

        let output = runtime.execute(&mut task).await.unwrap();
        assert_eq!(output.artifacts.len(), 1);
        assert!(output.artifacts[0].content.contains("Hello"));

        // Should have received a Completed report
        let report = rx.recv().await.unwrap();
        assert!(matches!(report, SubAgentReport::Completed { .. }));
    }

    #[tokio::test]
    async fn tool_call_then_text_response() {
        let mock_llm = Arc::new(MockLlmClient::new(vec![
            // First call: tool call
            LlmResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    tool_id: "search".into(),
                    arguments: serde_json::json!({"query": "test"}),
                }],
                prompt_tokens: 10,
                completion_tokens: 5,
            },
            // Second call: final text response
            LlmResponse {
                text: Some("Based on the search, here is the answer.".into()),
                tool_calls: vec![],
                prompt_tokens: 15,
                completion_tokens: 10,
            },
        ]));
        let mock_broker = Arc::new(MockToolBroker);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let runtime = AgentRuntime::new(test_config(), mock_llm, mock_broker, tx, 1);
        let mut task = Task::new(
            TriggerSource::Internal,
            "Search for something".into(),
            "test-agent".into(),
        );

        let output = runtime.execute(&mut task).await.unwrap();
        assert!(output.artifacts[0].content.contains("answer"));
        assert_eq!(output.token_usage.prompt_tokens, 25);
    }

    #[tokio::test]
    async fn max_iterations_exceeded() {
        // LLM always returns tool calls, never a final answer
        let responses: Vec<LlmResponse> = (0..10)
            .map(|i| LlmResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: format!("call-{i}"),
                    tool_id: "loop_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                prompt_tokens: 5,
                completion_tokens: 5,
            })
            .collect();

        let mock_llm = Arc::new(MockLlmClient::new(responses));
        let mock_broker = Arc::new(MockToolBroker);
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        let mut config = test_config();
        config.max_iterations = 3;

        let runtime = AgentRuntime::new(config, mock_llm, mock_broker, tx, 1);
        let mut task = Task::new(
            TriggerSource::Internal,
            "Infinite loop task".into(),
            "test-agent".into(),
        );

        let result = runtime.execute(&mut task).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NeuromancerError::Agent(AgentError::MaxIterationsExceeded { .. })
        ));
    }

    #[test]
    fn truncate_summary_handles_multibyte_unicode_safely() {
        let input = "I\u{2019}m testing \u{2014} unicode boundaries";
        let output = truncate_summary(input, 10);
        assert!(output.ends_with("..."));
        assert!(output.starts_with("I\u{2019}m testi"));
    }

    #[tokio::test]
    async fn execute_recovers_from_invalid_tool_call_error() {
        let llm = Arc::new(SequenceLlmClient::new(vec![
            SequenceItem::Error(invalid_tool_call_error("manage_bills")),
            SequenceItem::Response(LlmResponse {
                text: Some("Recovered response".into()),
                tool_calls: vec![],
                prompt_tokens: 0,
                completion_tokens: 0,
            }),
        ]));
        let broker = Arc::new(MockToolBroker);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let runtime = AgentRuntime::new(test_config(), llm, broker, tx, 1);
        let mut task = Task::new(
            TriggerSource::Internal,
            "Answer the question".into(),
            "test-agent".into(),
        );

        let output = runtime
            .execute(&mut task)
            .await
            .expect("recovery should succeed");
        assert!(output.artifacts[0].content.contains("Recovered response"));
    }

    #[tokio::test]
    async fn execute_fails_when_invalid_tool_call_retries_exhausted() {
        let llm = Arc::new(SequenceLlmClient::new(vec![
            SequenceItem::Error(invalid_tool_call_error("manage_bills")),
            SequenceItem::Error(invalid_tool_call_error("manage_accounts")),
        ]));
        let broker = Arc::new(MockToolBroker);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let runtime = AgentRuntime::new(test_config(), llm, broker, tx, 1);
        let mut task = Task::new(
            TriggerSource::Internal,
            "Answer the question".into(),
            "test-agent".into(),
        );

        let err = runtime
            .execute(&mut task)
            .await
            .expect_err("retries should be exhausted");
        assert!(
            err.to_string()
                .contains("invalid tool-call recovery exhausted after 1 retries"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execute_turn_recovers_from_invalid_tool_call_error() {
        use neuromancer_core::thread::{
            AgentThread, CrossReference, MessageId, ThreadId, ThreadStatus, ThreadStore,
        };
        use std::sync::Mutex;

        struct MockThreadStore {
            messages: Mutex<Vec<neuromancer_core::thread::ChatMessage>>,
        }

        impl MockThreadStore {
            fn new() -> Self {
                Self {
                    messages: Mutex::new(Vec::new()),
                }
            }
        }

        #[async_trait::async_trait]
        impl ThreadStore for MockThreadStore {
            async fn create_thread(&self, _thread: &AgentThread) -> Result<(), NeuromancerError> {
                Ok(())
            }
            async fn get_thread(
                &self,
                _thread_id: &ThreadId,
            ) -> Result<Option<AgentThread>, NeuromancerError> {
                Ok(None)
            }
            async fn update_status(
                &self,
                _thread_id: &ThreadId,
                _status: ThreadStatus,
            ) -> Result<(), NeuromancerError> {
                Ok(())
            }
            async fn list_threads_by_scope_type(
                &self,
                _scope_type: &str,
            ) -> Result<Vec<AgentThread>, NeuromancerError> {
                Ok(vec![])
            }
            async fn list_threads_for_agent(
                &self,
                _agent_id: &str,
            ) -> Result<Vec<AgentThread>, NeuromancerError> {
                Ok(vec![])
            }
            async fn append_messages(
                &self,
                _thread_id: &ThreadId,
                messages: &[neuromancer_core::thread::ChatMessage],
            ) -> Result<(), NeuromancerError> {
                self.messages.lock().unwrap().extend_from_slice(messages);
                Ok(())
            }
            async fn load_messages(
                &self,
                _thread_id: &ThreadId,
                _include_compacted: bool,
            ) -> Result<Vec<neuromancer_core::thread::ChatMessage>, NeuromancerError> {
                Ok(self.messages.lock().unwrap().clone())
            }
            async fn find_collaboration_thread(
                &self,
                _agent_id: &str,
                _parent_thread_id: &ThreadId,
            ) -> Result<Option<AgentThread>, NeuromancerError> {
                Ok(None)
            }
            async fn resolve_cross_reference(
                &self,
                _message_id: &MessageId,
            ) -> Result<Option<CrossReference>, NeuromancerError> {
                Ok(None)
            }
            async fn mark_compacted(
                &self,
                _thread_id: &ThreadId,
                _up_to_message_id: &MessageId,
            ) -> Result<(), NeuromancerError> {
                Ok(())
            }
            async fn total_uncompacted_tokens(
                &self,
                _thread_id: &ThreadId,
            ) -> Result<u32, NeuromancerError> {
                let msgs = self.messages.lock().unwrap();
                Ok(msgs.iter().map(|m| m.token_estimate).sum())
            }
        }

        let llm = Arc::new(SequenceLlmClient::new(vec![
            SequenceItem::Error(invalid_tool_call_error("manage_bills")),
            SequenceItem::Response(LlmResponse {
                text: Some("Recovered turn response".into()),
                tool_calls: vec![],
                prompt_tokens: 0,
                completion_tokens: 0,
            }),
        ]));
        let broker = Arc::new(MockToolBroker);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let runtime = AgentRuntime::new(test_config(), llm, broker, tx, 1);

        let thread_store = MockThreadStore::new();
        let thread_id = "test-thread".to_string();
        let output = runtime
            .execute_turn_with_thread_store(
                &thread_store,
                &thread_id,
                TriggerSource::Internal,
                "What happened?".into(),
                uuid::Uuid::new_v4(),
                vec![],
            )
            .await
            .expect("turn recovery should succeed");
        assert!(output.output.message.contains("Recovered turn response"));
    }
}
