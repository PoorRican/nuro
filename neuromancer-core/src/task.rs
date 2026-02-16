use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::AgentId;
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
        let now = Utc::now();
        Self {
            id: TaskId::new_v4(),
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Dispatched,
    Running,
    Completed,
    Failed,
    Cancelled,
    Suspended,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Dispatched | Self::Running)
    }
}

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
