use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::AgentId;
use crate::agent::TaskExecutionState;
use crate::trigger::TriggerSource;

pub type TaskId = uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub trigger_source: TriggerSource,
    pub instruction: String,
    pub assigned_agent: AgentId,
    pub state: TaskState,
    pub priority: TaskPriority,
    pub deadline: Option<DateTime<Utc>>,
    pub checkpoints: Vec<Checkpoint>,
    pub idempotency_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(
        trigger_source: TriggerSource,
        instruction: String,
        assigned_agent: AgentId,
    ) -> Self {
        Self::new_with_id(
            TaskId::new_v4(),
            trigger_source,
            instruction,
            assigned_agent,
        )
    }

    pub fn new_with_id(
        id: TaskId,
        trigger_source: TriggerSource,
        instruction: String,
        assigned_agent: AgentId,
    ) -> Self {
        let now = Utc::now();
        Self {
            id,
            parent_id: None,
            trigger_source,
            instruction,
            assigned_agent,
            state: TaskState::Queued,
            priority: TaskPriority::Normal,
            deadline: None,
            checkpoints: Vec::new(),
            idempotency_key: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_idempotency_key(mut self, key: String) -> Self {
        self.idempotency_key = Some(key);
        self
    }

    pub fn with_parent(mut self, parent_id: TaskId) -> Self {
        self.parent_id = Some(parent_id);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Dispatched,
    Running { execution_state: TaskExecutionState },
    Completed { output: TaskOutput },
    Failed { error: AgentErrorLike },
    Cancelled { reason: String },
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Dispatched | Self::Running { .. })
    }
}

// TODO: where is priority used?
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutput {
    pub artifacts: Vec<Artifact>,
    pub summary: String,
    pub token_usage: TokenUsage,
    pub duration: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub name: String,
    pub content: String,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Text,
    Code,
    File,
    Url,
    Data,
}

impl std::str::FromStr for ArtifactKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "text" => Ok(Self::Text),
            "code" => Ok(Self::Code),
            "file" => Ok(Self::File),
            "url" => Ok(Self::Url),
            "data" => Ok(Self::Data),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub task_id: TaskId,
    pub state_data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Serializable task failure payload for task terminal state persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentErrorLike {
    pub code: String,
    pub message: String,
}

impl AgentErrorLike {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Controls post-processing of an agent's terminal response.
#[derive(Debug, Clone, Copy, Default)]
pub enum OutputMode {
    /// Return text as-is (System0, UserConversation).
    #[default]
    Passthrough,
    /// Run extraction step to produce structured TaskOutput (task dispatch).
    Extract,
}

/// Extraction schema for structured output via LLM `output_schema`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskOutputSchema {
    pub summary: String,
    pub artifacts: Vec<ArtifactSchema>,
    pub no_reply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactSchema {
    pub kind: String,
    pub name: String,
    pub content: String,
}

impl TaskOutputSchema {
    pub fn into_task_output(self, token_usage: TokenUsage, duration: Duration) -> TaskOutput {
        TaskOutput {
            summary: self.summary,
            artifacts: self
                .artifacts
                .into_iter()
                .map(|a| Artifact {
                    kind: a.kind.parse().unwrap_or(ArtifactKind::Text),
                    name: a.name,
                    content: a.content,
                    mime_type: None,
                })
                .collect(),
            token_usage,
            duration,
        }
    }
}

/// Mode-agnostic terminal output from an agent — the raw final message before any
/// extraction or context-specific post-processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    pub message: String,
    pub token_usage: TokenUsage,
    pub duration: Duration,
}

impl AgentOutput {
    /// Convert to a TaskOutput by wrapping the message as a text artifact.
    /// This is the fallback when no structured extraction step is performed.
    pub fn into_task_output(self) -> TaskOutput {
        let summary_len = 200;
        let summary = if self.message.len() <= summary_len {
            self.message.clone()
        } else {
            let end = self
                .message
                .char_indices()
                .nth(summary_len)
                .map(|(i, _)| i)
                .unwrap_or(self.message.len());
            format!("{}...", &self.message[..end])
        };
        TaskOutput {
            artifacts: vec![Artifact {
                kind: ArtifactKind::Text,
                name: "response".to_string(),
                content: self.message,
                mime_type: Some("text/plain".to_string()),
            }],
            summary,
            token_usage: self.token_usage,
            duration: self.duration,
        }
    }
}

/// Result of a collaboration between agents, wrapping the raw output with a
/// cross-reference back to the collaboration thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollaborationResult {
    pub agent_output: AgentOutput,
    pub thread_ref: ThreadMessageRef,
}

/// Cross-reference to a specific message within a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessageRef {
    pub thread_id: String,
    pub message_id: uuid::Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_serde_roundtrip_and_helpers() {
        let task_id = uuid::Uuid::new_v4();
        let checkpoint = Checkpoint {
            task_id,
            state_data: serde_json::json!({ "step": "waiting" }),
            created_at: Utc::now(),
        };
        let output = TaskOutput {
            artifacts: vec![Artifact {
                kind: ArtifactKind::Text,
                name: "answer".to_string(),
                content: "done".to_string(),
                mime_type: Some("text/plain".to_string()),
            }],
            summary: "done".to_string(),
            token_usage: TokenUsage::default(),
            duration: Duration::from_millis(25),
        };

        let states = vec![
            TaskState::Queued,
            TaskState::Dispatched,
            TaskState::Running {
                execution_state: TaskExecutionState::Initializing { task_id },
            },
            TaskState::Running {
                execution_state: TaskExecutionState::Suspended {
                    checkpoint: checkpoint.clone(),
                    reason: "daemon shutdown".to_string(),
                },
            },
            TaskState::Completed {
                output: output.clone(),
            },
            TaskState::Failed {
                error: AgentErrorLike::new("failed", "boom"),
            },
            TaskState::Cancelled {
                reason: "user".to_string(),
            },
        ];

        for state in states {
            let encoded = serde_json::to_string(&state).expect("serialize");
            let decoded: TaskState = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(
                serde_json::to_value(&state).expect("to value"),
                serde_json::to_value(decoded).expect("to value")
            );
        }

        assert!(TaskState::Completed { output }.is_terminal());
        assert!(
            TaskState::Failed {
                error: AgentErrorLike::new("x", "y")
            }
            .is_terminal()
        );
        assert!(
            TaskState::Cancelled {
                reason: "cancelled".to_string()
            }
            .is_terminal()
        );
        assert!(
            TaskState::Running {
                execution_state: TaskExecutionState::Initializing { task_id }
            }
            .is_active()
        );
        assert!(
            !TaskState::Cancelled {
                reason: "cancelled".to_string()
            }
            .is_active()
        );
    }
}
