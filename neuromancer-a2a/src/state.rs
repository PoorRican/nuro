use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::{
    A2aArtifact, A2aMessage, A2aTaskRecord, A2aTaskStatus, AgentCard, Part,
};
use neuromancer_core::agent::SubAgentReport;
use neuromancer_core::task::TaskId;

/// Shared state for the A2A HTTP layer.
#[derive(Clone)]
pub struct A2aState {
    inner: Arc<A2aStateInner>,
}

struct A2aStateInner {
    /// Active A2A tasks indexed by their A2A task ID.
    tasks: RwLock<HashMap<String, A2aTaskRecord>>,
    /// Agent card served at /.well-known/agent-card.json.
    agent_card: RwLock<AgentCard>,
    /// Channel to submit new task requests to the orchestrator.
    task_sender: tokio::sync::mpsc::Sender<A2aTaskRequest>,
    /// SSE subscribers: task_id -> list of broadcast senders.
    subscribers: RwLock<HashMap<String, Vec<tokio::sync::broadcast::Sender<crate::types::StreamEvent>>>>,
}

/// A request submitted from the A2A layer to the orchestrator.
#[derive(Debug, Clone)]
pub struct A2aTaskRequest {
    pub a2a_task_id: String,
    pub message: A2aMessage,
    pub task_id: Option<String>,
}

impl A2aState {
    pub fn new(
        agent_card: AgentCard,
        task_sender: tokio::sync::mpsc::Sender<A2aTaskRequest>,
    ) -> Self {
        Self {
            inner: Arc::new(A2aStateInner {
                tasks: RwLock::new(HashMap::new()),
                agent_card: RwLock::new(agent_card),
                task_sender,
                subscribers: RwLock::new(HashMap::new()),
            }),
        }
    }

    pub async fn agent_card(&self) -> AgentCard {
        self.inner.agent_card.read().await.clone()
    }

    pub async fn update_agent_card(&self, card: AgentCard) {
        *self.inner.agent_card.write().await = card;
    }

    pub async fn get_task(&self, id: &str) -> Option<A2aTaskRecord> {
        self.inner.tasks.read().await.get(id).cloned()
    }

    pub async fn list_tasks(&self) -> Vec<A2aTaskRecord> {
        self.inner.tasks.read().await.values().cloned().collect()
    }

    /// Create a new A2A task record and return its ID.
    pub async fn create_task(&self, message: A2aMessage) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let record = A2aTaskRecord {
            id: id.clone(),
            status: A2aTaskStatus::Working,
            artifacts: Vec::new(),
            history: vec![message],
            internal_task_id: None,
            created_at: now,
            updated_at: now,
        };
        self.inner.tasks.write().await.insert(id.clone(), record);
        id
    }

    /// Link an A2A task to an internal TaskId.
    pub async fn link_internal_task(&self, a2a_id: &str, internal_id: TaskId) {
        if let Some(record) = self.inner.tasks.write().await.get_mut(a2a_id) {
            record.internal_task_id = Some(internal_id);
        }
    }

    /// Update task status from a SubAgentReport.
    pub async fn update_from_report(&self, a2a_id: &str, report: &SubAgentReport) {
        let mut tasks = self.inner.tasks.write().await;
        if let Some(record) = tasks.get_mut(a2a_id) {
            let (status, new_artifacts) = map_report_to_a2a(report);
            record.status = status;
            record.artifacts.extend(new_artifacts);
            record.updated_at = chrono::Utc::now();
        }
        drop(tasks);

        // Notify SSE subscribers.
        let subscribers = self.inner.subscribers.read().await;
        if let Some(senders) = subscribers.get(a2a_id) {
            let event = crate::types::StreamEvent::StatusUpdate {
                task_id: a2a_id.to_string(),
                status: map_report_status(report),
            };
            for sender in senders {
                let _ = sender.send(event.clone());
            }
        }
    }

    /// Update task status directly.
    pub async fn set_task_status(&self, a2a_id: &str, status: A2aTaskStatus) {
        if let Some(record) = self.inner.tasks.write().await.get_mut(a2a_id) {
            record.status = status;
            record.updated_at = chrono::Utc::now();
        }
    }

    /// Submit a task request to the orchestrator.
    pub async fn submit_task(&self, request: A2aTaskRequest) -> Result<(), String> {
        self.inner
            .task_sender
            .send(request)
            .await
            .map_err(|e| format!("failed to submit task: {e}"))
    }

    /// Subscribe to SSE events for a task. Returns a broadcast receiver.
    pub async fn subscribe(
        &self,
        task_id: &str,
    ) -> tokio::sync::broadcast::Receiver<crate::types::StreamEvent> {
        let mut subscribers = self.inner.subscribers.write().await;
        let senders = subscribers.entry(task_id.to_string()).or_default();
        let (tx, rx) = tokio::sync::broadcast::channel(64);
        senders.push(tx);
        rx
    }

    /// Notify subscribers that a task is done and clean up.
    pub async fn notify_done(&self, task_id: &str) {
        let subscribers = self.inner.subscribers.read().await;
        if let Some(senders) = subscribers.get(task_id) {
            let event = crate::types::StreamEvent::Done {
                task_id: task_id.to_string(),
            };
            for sender in senders {
                let _ = sender.send(event.clone());
            }
        }
    }
}

/// Map a SubAgentReport to an A2A task status and optional artifacts.
fn map_report_to_a2a(report: &SubAgentReport) -> (A2aTaskStatus, Vec<A2aArtifact>) {
    match report {
        SubAgentReport::Progress {
            artifacts_so_far, ..
        } => {
            let artifacts = artifacts_so_far
                .iter()
                .map(|a| A2aArtifact {
                    name: a.name.clone(),
                    parts: vec![Part::Text {
                        text: a.content.clone(),
                    }],
                    index: None,
                    last_chunk: None,
                    metadata: None,
                })
                .collect();
            (A2aTaskStatus::Working, artifacts)
        }
        SubAgentReport::InputRequired { .. } => (A2aTaskStatus::InputRequired, vec![]),
        SubAgentReport::Completed { artifacts, .. } => {
            let a2a_artifacts = artifacts
                .iter()
                .map(|a| A2aArtifact {
                    name: a.name.clone(),
                    parts: vec![Part::Text {
                        text: a.content.clone(),
                    }],
                    index: None,
                    last_chunk: Some(true),
                    metadata: None,
                })
                .collect();
            (A2aTaskStatus::Completed, a2a_artifacts)
        }
        SubAgentReport::Failed { .. } => (A2aTaskStatus::Failed, vec![]),
        SubAgentReport::Stuck { partial_result, .. } => {
            let artifacts = partial_result
                .iter()
                .map(|a| A2aArtifact {
                    name: a.name.clone(),
                    parts: vec![Part::Text {
                        text: a.content.clone(),
                    }],
                    index: None,
                    last_chunk: Some(true),
                    metadata: None,
                })
                .collect();
            (A2aTaskStatus::Failed, artifacts)
        }
        SubAgentReport::ToolFailure {
            retry_eligible, ..
        } => {
            if *retry_eligible {
                (A2aTaskStatus::Working, vec![])
            } else {
                (A2aTaskStatus::Failed, vec![])
            }
        }
        SubAgentReport::PolicyDenied { .. } => (A2aTaskStatus::InputRequired, vec![]),
    }
}

fn map_report_status(report: &SubAgentReport) -> A2aTaskStatus {
    map_report_to_a2a(report).0
}
