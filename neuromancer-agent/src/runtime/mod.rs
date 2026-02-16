//! `AgentRuntime` state machine: LLM completions + tool execution until final response.

mod helpers;
mod recovery;

use std::sync::Arc;
use std::time::Instant;

use neuromancer_core::agent::{AgentConfig, SubAgentReport, TaskExecutionState};
use neuromancer_core::error::{AgentError, NeuromancerError};
use neuromancer_core::task::{
    Artifact, ArtifactKind, Checkpoint, Task, TaskOutput, TaskState, TokenUsage,
};
use neuromancer_core::tool::{AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult};
use neuromancer_core::trigger::TriggerSource;

use crate::conversation::{ChatMessage, ConversationContext, TruncationStrategy};
use crate::llm::LlmClient;
use crate::session::{AgentSessionId, InMemorySessionStore};

use helpers::{available_tool_names, specs_to_rig_definitions, truncate_summary};

#[derive(Debug, Clone)]
pub struct TurnExecutionResult {
    pub task_id: uuid::Uuid,
    pub output: TaskOutput,
}

/// Agent runtime that manages a single task execution through the state machine.
pub struct AgentRuntime {
    config: AgentConfig,
    llm_client: Arc<dyn LlmClient>,
    tool_broker: Arc<dyn ToolBroker>,
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
            report_tx,
            tool_call_retry_limit,
        }
    }

    // TODO: there seems to be significant code duplication between `execute` and `execute_turn`

    /// Execute a task through the full state machine lifecycle.
    /// Returns the final TaskOutput on success.
    pub async fn execute(&self, task: &mut Task) -> Result<TaskOutput, NeuromancerError> {
        let start = Instant::now();
        let mut total_usage = TokenUsage::default();

        // Transition: Initializing
        task.state = TaskState::Running;
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
                    reason: format!("max iterations ({max_iterations}) exceeded"),
                    partial_result: None,
                })
                .await;
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
                artifacts: output.artifacts.clone(),
                summary: output.summary.clone(),
            })
            .await;

            return Ok(output);
        }
    }

    /// Execute a single conversational turn using persistent agent-owned session history.
    pub async fn execute_turn(
        &self,
        session_store: &InMemorySessionStore,
        session_id: AgentSessionId,
        source: TriggerSource,
        user_message: String,
    ) -> Result<TurnExecutionResult, NeuromancerError> {
        let mut task = Task::new(source, user_message.clone(), self.config.id.clone());
        let start = Instant::now();
        let mut total_usage = TokenUsage::default();

        task.state = TaskState::Running;
        tracing::info!(
            task_id = %task.id,
            agent_id = %self.config.id,
            "agent runtime turn execution starting"
        );

        let agent_ctx = AgentContext {
            agent_id: self.config.id.clone(),
            task_id: task.id,
            allowed_tools: self.config.capabilities.skills.clone(),
            allowed_mcp_servers: self.config.capabilities.mcp_servers.clone(),
            allowed_secrets: self.config.capabilities.secrets.clone(),
            allowed_memory_partitions: self.config.capabilities.memory_partitions.clone(),
        };

        let system_prompt = self.config.system_prompt.clone();
        let default_conversation = {
            let mut conversation = ConversationContext::new(
                u32::MAX,
                TruncationStrategy::SlidingWindow { keep_last: 50 },
            );
            conversation.add_message(ChatMessage::system(&system_prompt));
            conversation
        };

        let session = session_store
            .load_or_create(session_id, default_conversation)
            .await;
        let mut conversation = session.conversation.clone();
        conversation.add_message(ChatMessage::user(user_message));

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
                    reason: format!("max iterations ({max_iterations}) exceeded"),
                    partial_result: None,
                })
                .await;
                break Err(NeuromancerError::Agent(err));
            }

            tracing::debug!(
                task_id = %task.id,
                iteration,
                "thinking: calling LLM (session turn)"
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
                artifacts: output.artifacts.clone(),
                summary: output.summary.clone(),
            })
            .await;

            break Ok(output);
        };

        session_store
            .save_conversation(session_id, conversation)
            .await;

        run_result.map(|output| TurnExecutionResult {
            task_id: task.id,
            output,
        })
    }

    async fn execute_tool_call(&self, ctx: &AgentContext, call: &ToolCall) -> ToolResult {
        match self.tool_broker.call_tool(ctx, call.clone()).await {
            Ok(result) => result,
            Err(e) => ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Error(e.to_string()),
            },
        }
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

        let session_store = InMemorySessionStore::new();
        let session_id = uuid::Uuid::new_v4();
        let output = runtime
            .execute_turn(
                &session_store,
                session_id,
                TriggerSource::Internal,
                "What happened?".into(),
            )
            .await
            .expect("turn recovery should succeed");
        assert!(
            output.output.artifacts[0]
                .content
                .contains("Recovered turn response")
        );
    }
}
