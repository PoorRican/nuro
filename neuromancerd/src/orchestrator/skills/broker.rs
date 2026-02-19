use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use neuromancer_core::agent::TaskExecutionState;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::task::{
    AgentOutput, Checkpoint, CollaborationResult, Task, TaskPriority, TaskState, ThreadMessageRef,
};
use neuromancer_core::thread::{
    AgentThread, CompactionPolicy, ThreadScope, ThreadStatus, ThreadStore, TruncationStrategy,
};
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolResult, ToolSource, ToolSpec,
};
use neuromancer_core::trigger::{TriggerSource, TriggerType};
use neuromancer_skills::SkillToolBroker;

use crate::orchestrator::state::task_manager::running_state;
use crate::orchestrator::state::{PersistedCheckpoint, TaskExecutionContext, TaskManager};

#[derive(Clone)]
pub(crate) struct OrchestratorSkillBroker {
    skill_broker: SkillToolBroker,
    task_manager: TaskManager,
    thread_store: Arc<dyn ThreadStore>,
}

impl OrchestratorSkillBroker {
    pub(crate) fn new(
        skill_broker: SkillToolBroker,
        task_manager: TaskManager,
        thread_store: Arc<dyn ThreadStore>,
    ) -> Self {
        Self {
            skill_broker,
            task_manager,
            thread_store,
        }
    }

    fn assistance_tool_spec() -> ToolSpec {
        ToolSpec {
            id: "request_agent_assistance".to_string(),
            name: "request_agent_assistance".to_string(),
            description: "Request help from an allowed peer agent. args: { target_agent, instruction, context?, timeout_secs? }".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_agent": {"type": "string"},
                    "instruction": {"type": "string"},
                    "context": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1}
                },
                "required": ["target_agent", "instruction"],
                "additionalProperties": false
            }),
            source: ToolSource::Builtin,
        }
    }

    /// Find an existing collaboration thread or create a new one.
    /// The collaboration thread is keyed by (target_agent, parent_thread_id).
    async fn find_or_create_collaboration_thread(
        &self,
        target_agent: &str,
        parent_thread_id: &str,
    ) -> Result<AgentThread, NeuromancerError> {
        let parent_id = parent_thread_id.to_string();
        if let Some(existing) = self
            .thread_store
            .find_collaboration_thread(target_agent, &parent_id)
            .await?
        {
            tracing::debug!(
                thread_id = %existing.id,
                target_agent = %target_agent,
                parent_thread_id = %parent_thread_id,
                "collaboration_thread_reused"
            );
            return Ok(existing);
        }

        let thread_id = format!(
            "collab-{}-{}",
            target_agent,
            uuid::Uuid::new_v4()
                .to_string()
                .chars()
                .take(8)
                .collect::<String>()
        );
        let now = Utc::now();
        let thread = AgentThread {
            id: thread_id.clone(),
            agent_id: target_agent.to_string(),
            scope: ThreadScope::Collaboration {
                parent_thread_id: parent_thread_id.to_string(),
                root_scope: Box::new(ThreadScope::Task {
                    task_id: parent_thread_id.to_string(),
                }),
            },
            compaction_policy: CompactionPolicy::InPlace {
                strategy: TruncationStrategy::default(),
            },
            context_window_budget: 128_000,
            status: ThreadStatus::Active,
            created_at: now,
            updated_at: now,
        };

        self.thread_store.create_thread(&thread).await?;

        tracing::info!(
            thread_id = %thread_id,
            target_agent = %target_agent,
            parent_thread_id = %parent_thread_id,
            "collaboration_thread_created"
        );

        Ok(thread)
    }
}

#[async_trait::async_trait]
impl ToolBroker for OrchestratorSkillBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let mut tools = self.skill_broker.list_tools(ctx).await;
        if !ctx.allowed_peer_agents.is_empty() {
            tools.push(Self::assistance_tool_spec());
        }
        tools
    }

    async fn call_tool(
        &self,
        ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        if call.tool_id != "request_agent_assistance" {
            return self.skill_broker.call_tool(ctx, call).await;
        }

        let target_agent = call
            .arguments
            .get("target_agent")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: call.tool_id.clone(),
                    message: "missing 'target_agent'".to_string(),
                })
            })?
            .to_string();
        if !ctx
            .allowed_peer_agents
            .iter()
            .any(|peer| peer == &target_agent)
        {
            return Err(NeuromancerError::Tool(ToolError::CapabilityDenied {
                agent_id: ctx.agent_id.clone(),
                capability: format!("can_request:{target_agent}"),
            }));
        }

        let instruction = call
            .arguments
            .get("instruction")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: call.tool_id.clone(),
                    message: "missing 'instruction'".to_string(),
                })
            })?
            .to_string();
        let timeout_secs = call
            .arguments
            .get("timeout_secs")
            .and_then(|value| value.as_u64())
            .unwrap_or(300);
        let context = call
            .arguments
            .get("context")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned);
        let child_instruction = match context {
            Some(context) => format!("{instruction}\n\nContext:\n{context}"),
            None => instruction,
        };

        // Resolve the caller's thread_id from execution context for collaboration thread linkage.
        let caller_thread_id = self
            .task_manager
            .execution_context_for(ctx.task_id)
            .await
            .and_then(|exec_ctx| exec_ctx.thread_id);

        // Find or create a collaboration thread for the target agent.
        let collaboration_thread = if let Some(parent_thread_id) = &caller_thread_id {
            Some(
                self.find_or_create_collaboration_thread(&target_agent, parent_thread_id)
                    .await?,
            )
        } else {
            None
        };
        let collab_thread_id = collaboration_thread
            .as_ref()
            .map(|thread| thread.id.clone());

        // Caller enters WaitingForCollaboration while the child task runs.
        let wait_checkpoint = collaboration_wait_checkpoint(
            ctx.task_id,
            &target_agent,
            collab_thread_id.as_deref().unwrap_or("unknown"),
            &call.id,
        );
        if let Err(err) = self
            .task_manager
            .transition(
                ctx.task_id,
                Some("running"),
                running_state(wait_checkpoint.execution_state.clone()),
                Some(wait_checkpoint),
            )
            .await
        {
            tracing::debug!(
                task_id = %ctx.task_id,
                error = ?err,
                "caller_wait_state_transition_skipped"
            );
        }

        let mut task = Task::new(
            TriggerSource::Internal,
            child_instruction,
            target_agent.clone(),
        );
        task.parent_id = Some(ctx.task_id);
        task.priority = TaskPriority::Normal;
        task.state = TaskState::Queued;

        // Register execution context so the task worker uses the collaboration thread.
        if let Some(thread_id) = &collab_thread_id {
            self.task_manager
                .register_execution_context(
                    task.id,
                    TaskExecutionContext {
                        turn_id: None,
                        call_id: Some(call.id.clone()),
                        thread_id: Some(thread_id.clone()),
                        trigger_type: TriggerType::Internal,
                        publish_output: false,
                    },
                )
                .await;
        }

        let task_id = self.task_manager.enqueue_direct(task).await?;
        let result = self
            .task_manager
            .await_task_result(task_id, Duration::from_secs(timeout_secs))
            .await;

        // Move caller back into active execution once assistance wait ends.
        if let Err(err) = self
            .task_manager
            .transition(
                ctx.task_id,
                Some("running"),
                running_state(TaskExecutionState::Thinking {
                    conversation_len: 0,
                    iteration: 0,
                }),
                None,
            )
            .await
        {
            tracing::debug!(
                task_id = %ctx.task_id,
                error = ?err,
                "caller_resume_state_transition_skipped"
            );
        }

        let output = result?;

        // Wrap result with CollaborationResult when a collaboration thread exists,
        // providing a cross-reference back to the collaboration thread.
        let result_json = if let Some(ref thread_id) = collab_thread_id {
            let collab_result = CollaborationResult {
                agent_output: AgentOutput {
                    message: output.summary.clone(),
                    token_usage: output.token_usage.clone(),
                    duration: output.duration,
                },
                thread_ref: ThreadMessageRef {
                    thread_id: thread_id.clone(),
                    message_id: uuid::Uuid::nil(),
                },
            };
            serde_json::to_value(&collab_result).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: "request_agent_assistance".to_string(),
                    message: err.to_string(),
                })
            })?
        } else {
            serde_json::to_value(&output).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: "request_agent_assistance".to_string(),
                    message: err.to_string(),
                })
            })?
        };

        Ok(ToolResult {
            call_id: call.id,
            output: neuromancer_core::tool::ToolOutput::Success(result_json),
        })
    }
}

fn collaboration_wait_checkpoint(
    task_id: uuid::Uuid,
    target_agent: &str,
    collaboration_thread_id: &str,
    call_id: &str,
) -> PersistedCheckpoint {
    let created_at = Utc::now();
    let marker = Checkpoint {
        task_id,
        state_data: serde_json::json!({
            "event": "request_agent_assistance_wait",
            "target_agent": target_agent,
            "collaboration_thread_id": collaboration_thread_id,
        }),
        created_at,
    };
    let execution_state = TaskExecutionState::WaitingForCollaboration {
        thread_id: collaboration_thread_id.to_string(),
        target_agent: target_agent.to_string(),
        collaboration_call_id: call_id.to_string(),
        checkpoint: marker,
    };

    PersistedCheckpoint {
        task_id,
        created_at,
        execution_state,
        conversation_json: None,
        reason: Some("awaiting request_agent_assistance".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use neuromancer_core::error::{AgentError, ToolError};
    use neuromancer_core::task::{Artifact, ArtifactKind, TaskOutput, TokenUsage};
    use neuromancer_core::tool::{ToolBroker, ToolCall, ToolOutput};
    use neuromancer_skills::SkillRegistry;

    use neuromancer_core::task::CollaborationResult;

    use crate::orchestrator::security::execution_guard::PlaceholderExecutionGuard;
    use crate::orchestrator::state::{TaskManager, TaskStore};
    use crate::orchestrator::threads::SqliteThreadStore;

    async fn test_manager() -> TaskManager {
        let store = TaskStore::in_memory().await.expect("store");
        TaskManager::new(store).await.expect("task manager")
    }

    async fn test_thread_store() -> Arc<dyn ThreadStore> {
        Arc::new(
            SqliteThreadStore::in_memory()
                .await
                .expect("thread store"),
        )
    }

    fn test_skill_broker() -> SkillToolBroker {
        let registry = SkillRegistry::new(vec![]);
        SkillToolBroker::new(
            "planner",
            &[],
            &registry,
            PathBuf::from("."),
            Arc::new(PlaceholderExecutionGuard),
        )
        .expect("skill broker")
    }

    fn test_ctx(allowed_peers: Vec<String>) -> AgentContext {
        AgentContext {
            agent_id: "planner".to_string(),
            task_id: uuid::Uuid::new_v4(),
            allowed_tools: vec![],
            allowed_mcp_servers: vec![],
            allowed_peer_agents: allowed_peers,
            allowed_secrets: vec![],
            allowed_memory_partitions: vec![],
        }
    }

    fn completed_output(summary: &str) -> TaskOutput {
        TaskOutput {
            artifacts: vec![Artifact {
                kind: ArtifactKind::Text,
                name: "response".to_string(),
                content: summary.to_string(),
                mime_type: Some("text/plain".to_string()),
            }],
            summary: summary.to_string(),
            token_usage: TokenUsage::default(),
            duration: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn denied_target_returns_capability_denied() {
        let manager = test_manager().await;
        let thread_store = test_thread_store().await;
        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager, thread_store);
        let ctx = test_ctx(vec!["browser".to_string()]);

        let err = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "writer",
                        "instruction": "draft a summary",
                    }),
                },
            )
            .await
            .expect_err("capability should be denied");

        assert!(matches!(
            err,
            NeuromancerError::Tool(ToolError::CapabilityDenied { .. })
        ));
    }

    #[tokio::test]
    async fn assistance_timeout_is_propagated() {
        let manager = test_manager().await;
        let thread_store = test_thread_store().await;
        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager, thread_store);
        let ctx = test_ctx(vec!["browser".to_string()]);

        let err = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "fetch data",
                        "timeout_secs": 0,
                    }),
                },
            )
            .await
            .expect_err("await should timeout");
        assert!(matches!(
            err,
            NeuromancerError::Agent(AgentError::Timeout { .. })
        ));
    }

    #[tokio::test]
    async fn assistance_success_returns_child_output() {
        let manager = test_manager().await;
        let manager_for_worker = manager.clone();
        tokio::spawn(async move {
            let child = manager_for_worker
                .wait_for_next_task()
                .await
                .expect("child task");
            manager_for_worker
                .transition(
                    child.id,
                    Some("queued"),
                    TaskState::Completed {
                        output: completed_output("from helper"),
                    },
                    None,
                )
                .await
                .expect("complete child");
        });

        let thread_store = test_thread_store().await;
        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager, thread_store);
        let ctx = AgentContext {
            task_id: uuid::Uuid::new_v4(),
            ..test_ctx(vec!["browser".to_string()])
        };
        let result = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "fetch data",
                        "timeout_secs": 5,
                    }),
                },
            )
            .await
            .expect("assistance should succeed");

        let ToolOutput::Success(payload) = result.output else {
            panic!("expected success payload");
        };
        assert_eq!(payload["summary"], serde_json::json!("from helper"));
    }

    #[tokio::test]
    async fn collaboration_thread_is_created_when_caller_has_context() {
        let manager = test_manager().await;
        let thread_store = test_thread_store().await;
        let caller_task_id = uuid::Uuid::new_v4();
        let caller_thread_id = format!("planner-{}", &caller_task_id.to_string()[..8]);

        // Create the caller's thread in the store.
        let now = Utc::now();
        thread_store
            .create_thread(&AgentThread {
                id: caller_thread_id.clone(),
                agent_id: "planner".to_string(),
                scope: ThreadScope::Task {
                    task_id: caller_task_id.to_string(),
                },
                compaction_policy: CompactionPolicy::InPlace {
                    strategy: TruncationStrategy::default(),
                },
                context_window_budget: 128_000,
                status: ThreadStatus::Active,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("create caller thread");

        // Register execution context so the broker can resolve the caller's thread_id.
        manager
            .register_execution_context(
                caller_task_id,
                TaskExecutionContext {
                    turn_id: None,
                    call_id: None,
                    thread_id: Some(caller_thread_id.clone()),
                    trigger_type: TriggerType::Internal,
                    publish_output: false,
                },
            )
            .await;

        // Spawn a worker to complete the child task.
        let manager_for_worker = manager.clone();
        let thread_store_for_check = thread_store.clone();
        let check_handle = tokio::spawn(async move {
            let child = manager_for_worker
                .wait_for_next_task()
                .await
                .expect("child task");

            // Verify that the child task's execution context has a collaboration thread_id.
            let child_ctx = manager_for_worker
                .execution_context_for(child.id)
                .await
                .expect("child execution context");
            let collab_thread_id = child_ctx.thread_id.expect("collaboration thread_id");
            assert!(
                collab_thread_id.starts_with("collab-browser-"),
                "expected collaboration thread prefix, got: {collab_thread_id}"
            );

            // Verify the collaboration thread exists in ThreadStore.
            let collab_thread = thread_store_for_check
                .get_thread(&collab_thread_id)
                .await
                .expect("thread lookup")
                .expect("collaboration thread should exist");
            assert_eq!(collab_thread.agent_id, "browser");
            assert!(matches!(
                collab_thread.scope,
                ThreadScope::Collaboration { .. }
            ));

            manager_for_worker
                .transition(
                    child.id,
                    Some("queued"),
                    TaskState::Completed {
                        output: completed_output("collaborated"),
                    },
                    None,
                )
                .await
                .expect("complete child");

            collab_thread_id
        });

        let broker =
            OrchestratorSkillBroker::new(test_skill_broker(), manager, thread_store.clone());
        let ctx = AgentContext {
            task_id: caller_task_id,
            ..test_ctx(vec!["browser".to_string()])
        };
        let result = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "fetch data",
                        "timeout_secs": 5,
                    }),
                },
            )
            .await
            .expect("assistance should succeed");

        let ToolOutput::Success(payload) = result.output else {
            panic!("expected success payload");
        };
        let collab_result: CollaborationResult =
            serde_json::from_value(payload).expect("should deserialize as CollaborationResult");
        assert_eq!(collab_result.agent_output.message, "collaborated");

        // Verify collaboration thread was created with correct properties.
        let collab_thread_id = check_handle.await.expect("worker handle");

        // Verify that find_collaboration_thread returns the same thread.
        let found = thread_store
            .find_collaboration_thread("browser", &caller_thread_id)
            .await
            .expect("find_collaboration_thread")
            .expect("should find existing collaboration thread");
        assert_eq!(found.id, collab_thread_id);
    }

    #[tokio::test]
    async fn integration_collaboration_caller_state_transitions() {
        use neuromancer_core::agent::TaskExecutionState;
        use neuromancer_core::task::TaskState;

        let manager = test_manager().await;
        let thread_store = test_thread_store().await;
        let caller_task_id = uuid::Uuid::new_v4();
        let caller_thread_id = format!("planner-{}", &caller_task_id.to_string()[..8]);

        // Create the caller's thread + execution context.
        let now = Utc::now();
        thread_store
            .create_thread(&AgentThread {
                id: caller_thread_id.clone(),
                agent_id: "planner".to_string(),
                scope: ThreadScope::Task {
                    task_id: caller_task_id.to_string(),
                },
                compaction_policy: CompactionPolicy::InPlace {
                    strategy: TruncationStrategy::default(),
                },
                context_window_budget: 128_000,
                status: ThreadStatus::Active,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("create caller thread");
        manager
            .register_execution_context(
                caller_task_id,
                TaskExecutionContext {
                    turn_id: None,
                    call_id: None,
                    thread_id: Some(caller_thread_id.clone()),
                    trigger_type: TriggerType::Internal,
                    publish_output: false,
                },
            )
            .await;

        // Put the caller task into a Running state so transitions are valid.
        let mut caller_task = neuromancer_core::task::Task::new(
            neuromancer_core::trigger::TriggerSource::Internal,
            "test".to_string(),
            "planner".to_string(),
        );
        caller_task.id = caller_task_id;
        manager.enqueue_direct(caller_task).await.expect("enqueue caller");
        // Drain the queue so it's picked up.
        let _ = manager.wait_for_next_task().await;
        manager
            .transition(
                caller_task_id,
                Some("queued"),
                TaskState::Running {
                    execution_state: TaskExecutionState::Thinking {
                        conversation_len: 0,
                        iteration: 0,
                    },
                },
                None,
            )
            .await
            .expect("transition to running");

        // Spawn a worker that verifies WaitingForCollaboration, then completes the child.
        let manager_for_worker = manager.clone();
        tokio::spawn(async move {
            let child = manager_for_worker
                .wait_for_next_task()
                .await
                .expect("child task");

            // At this point the caller should be in WaitingForCollaboration.
            let caller = manager_for_worker
                .get_task(caller_task_id)
                .await
                .expect("get caller")
                .expect("caller exists");
            match &caller.state {
                TaskState::Running { execution_state } => {
                    assert!(
                        matches!(
                            execution_state,
                            TaskExecutionState::WaitingForCollaboration { .. }
                        ),
                        "expected WaitingForCollaboration, got: {execution_state:?}"
                    );
                }
                other => panic!("expected Running state, got: {other:?}"),
            }

            manager_for_worker
                .transition(
                    child.id,
                    Some("queued"),
                    TaskState::Completed {
                        output: completed_output("done"),
                    },
                    None,
                )
                .await
                .expect("complete child");
        });

        let broker =
            OrchestratorSkillBroker::new(test_skill_broker(), manager.clone(), thread_store);
        let ctx = AgentContext {
            task_id: caller_task_id,
            ..test_ctx(vec!["browser".to_string()])
        };
        let _result = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "help me",
                        "timeout_secs": 5,
                    }),
                },
            )
            .await
            .expect("assistance should succeed");

        // After call_tool returns, caller should be back in Thinking state.
        let caller_after = manager
            .get_task(caller_task_id)
            .await
            .expect("get caller")
            .expect("caller exists");
        match &caller_after.state {
            TaskState::Running { execution_state } => {
                assert!(
                    matches!(
                        execution_state,
                        TaskExecutionState::Thinking { .. }
                    ),
                    "expected Thinking after collaboration, got: {execution_state:?}"
                );
            }
            other => panic!("expected Running state after collaboration, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn collaboration_thread_is_reused_across_calls() {
        let manager = test_manager().await;
        let thread_store = test_thread_store().await;
        let caller_task_id = uuid::Uuid::new_v4();
        let caller_thread_id = format!("planner-{}", &caller_task_id.to_string()[..8]);

        let now = Utc::now();
        thread_store
            .create_thread(&AgentThread {
                id: caller_thread_id.clone(),
                agent_id: "planner".to_string(),
                scope: ThreadScope::Task {
                    task_id: caller_task_id.to_string(),
                },
                compaction_policy: CompactionPolicy::InPlace {
                    strategy: TruncationStrategy::default(),
                },
                context_window_budget: 128_000,
                status: ThreadStatus::Active,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("create caller thread");

        manager
            .register_execution_context(
                caller_task_id,
                TaskExecutionContext {
                    turn_id: None,
                    call_id: None,
                    thread_id: Some(caller_thread_id.clone()),
                    trigger_type: TriggerType::Internal,
                    publish_output: false,
                },
            )
            .await;

        // First call: spawn worker, collect collab thread id
        let manager_w1 = manager.clone();
        tokio::spawn(async move {
            let child = manager_w1.wait_for_next_task().await.expect("child1");
            manager_w1
                .transition(
                    child.id,
                    Some("queued"),
                    TaskState::Completed {
                        output: completed_output("first"),
                    },
                    None,
                )
                .await
                .expect("complete child1");
        });

        let broker = OrchestratorSkillBroker::new(
            test_skill_broker(),
            manager.clone(),
            thread_store.clone(),
        );
        let ctx = AgentContext {
            task_id: caller_task_id,
            ..test_ctx(vec!["browser".to_string()])
        };

        let result1 = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-1".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "first request",
                        "timeout_secs": 5,
                    }),
                },
            )
            .await
            .expect("first assistance");
        let ToolOutput::Success(payload1) = result1.output else {
            panic!("expected success");
        };
        let collab1: CollaborationResult =
            serde_json::from_value(payload1).expect("deserialize result1");
        let thread_id_1 = collab1.thread_ref.thread_id;

        // Second call: same caller context → should reuse the collaboration thread
        let manager_w2 = manager.clone();
        tokio::spawn(async move {
            let child = manager_w2.wait_for_next_task().await.expect("child2");
            manager_w2
                .transition(
                    child.id,
                    Some("queued"),
                    TaskState::Completed {
                        output: completed_output("second"),
                    },
                    None,
                )
                .await
                .expect("complete child2");
        });

        let result2 = broker
            .call_tool(
                &ctx,
                ToolCall {
                    id: "call-2".to_string(),
                    tool_id: "request_agent_assistance".to_string(),
                    arguments: serde_json::json!({
                        "target_agent": "browser",
                        "instruction": "second request",
                        "timeout_secs": 5,
                    }),
                },
            )
            .await
            .expect("second assistance");
        let ToolOutput::Success(payload2) = result2.output else {
            panic!("expected success");
        };
        let collab2: CollaborationResult =
            serde_json::from_value(payload2).expect("deserialize result2");
        let thread_id_2 = collab2.thread_ref.thread_id;

        // The thread IDs should be the same — reused, not created new
        assert_eq!(thread_id_1, thread_id_2, "collaboration thread should be reused");
    }
}
