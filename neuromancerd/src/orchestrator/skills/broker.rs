use std::time::Duration;

use chrono::Utc;
use neuromancer_core::agent::TaskExecutionState;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::task::{Checkpoint, Task, TaskPriority, TaskState};
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolResult, ToolSource, ToolSpec,
};
use neuromancer_core::trigger::TriggerSource;
use neuromancer_skills::SkillToolBroker;

use crate::orchestrator::state::task_manager::running_state;
use crate::orchestrator::state::{PersistedCheckpoint, TaskManager};

#[derive(Clone)]
pub(crate) struct OrchestratorSkillBroker {
    skill_broker: SkillToolBroker,
    task_manager: TaskManager,
}

impl OrchestratorSkillBroker {
    pub(crate) fn new(skill_broker: SkillToolBroker, task_manager: TaskManager) -> Self {
        Self {
            skill_broker,
            task_manager,
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

        // Caller enters WaitingForInput while the child task runs.
        let wait_checkpoint = assistance_wait_checkpoint(ctx.task_id, &target_agent);
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

        let mut task = Task::new(TriggerSource::Internal, child_instruction, target_agent);
        task.parent_id = Some(ctx.task_id);
        task.priority = TaskPriority::Normal;
        task.state = TaskState::Queued;

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

        Ok(ToolResult {
            call_id: call.id,
            output: neuromancer_core::tool::ToolOutput::Success(
                serde_json::to_value(output).map_err(|err| {
                    NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "request_agent_assistance".to_string(),
                        message: err.to_string(),
                    })
                })?,
            ),
        })
    }
}

fn assistance_wait_checkpoint(task_id: uuid::Uuid, target_agent: &str) -> PersistedCheckpoint {
    let created_at = Utc::now();
    let marker = Checkpoint {
        task_id,
        state_data: serde_json::json!({
            "event": "request_agent_assistance_wait",
            "target_agent": target_agent,
        }),
        created_at,
    };
    let execution_state = TaskExecutionState::WaitingForInput {
        question: format!("Awaiting assistance from '{target_agent}'"),
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

    use crate::orchestrator::security::execution_guard::PlaceholderExecutionGuard;
    use crate::orchestrator::state::{TaskManager, TaskStore};

    async fn test_manager() -> TaskManager {
        let store = TaskStore::in_memory().await.expect("store");
        TaskManager::new(store).await.expect("task manager")
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
        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager);
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
        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager);
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

        let broker = OrchestratorSkillBroker::new(test_skill_broker(), manager);
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
}
