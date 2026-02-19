use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use neuromancer_agent::session::AgentSessionId;
use neuromancer_core::agent::{AgentHealthConfig, CircuitBreakerState, TaskExecutionState};
use neuromancer_core::error::{AgentError, InfraError, NeuromancerError, ToolError};
use neuromancer_core::task::{AgentErrorLike, Checkpoint, Task, TaskId, TaskOutput, TaskState};
use neuromancer_core::trigger::TriggerType;
use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};

use super::task_store::{PersistedCheckpoint, TaskStore, task_state_label};

#[derive(Clone)]
pub(crate) struct TaskManager {
    store: TaskStore,
    notify: Arc<Notify>,
    state: Arc<AsyncMutex<TaskManagerState>>,
}

struct TaskManagerState {
    tasks: HashMap<TaskId, Task>,
    queue: BinaryHeap<QueueItem>,
    queued_ids: HashSet<TaskId>,
    waiters: HashMap<TaskId, Vec<oneshot::Sender<Result<TaskOutput, String>>>>,
    accepting_new_tasks: bool,
    last_heartbeat: HashMap<TaskId, chrono::DateTime<Utc>>,
    health_config_by_agent: HashMap<String, AgentHealthConfig>,
    circuit_breakers: HashMap<String, CircuitBreakerState>,
    execution_contexts: HashMap<TaskId, TaskExecutionContext>,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskExecutionContext {
    pub turn_id: Option<uuid::Uuid>,
    pub call_id: Option<String>,
    pub thread_id: Option<String>,
    pub trigger_type: TriggerType,
    pub session_id: Option<AgentSessionId>,
    pub persisted_message_count: usize,
    pub publish_output: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WatchdogCandidate {
    pub task_id: TaskId,
    pub agent_id: String,
    pub elapsed: Duration,
    pub watchdog_timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueueItem {
    task_id: TaskId,
    priority: i64,
    created_at: chrono::DateTime<Utc>,
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            // older items should win for same priority
            .then_with(|| other.created_at.cmp(&self.created_at))
            .then_with(|| self.task_id.cmp(&other.task_id))
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl TaskManager {
    pub(crate) async fn new(store: TaskStore) -> Result<Self, NeuromancerError> {
        let manager = Self {
            store,
            notify: Arc::new(Notify::new()),
            state: Arc::new(AsyncMutex::new(TaskManagerState {
                tasks: HashMap::new(),
                queue: BinaryHeap::new(),
                queued_ids: HashSet::new(),
                waiters: HashMap::new(),
                accepting_new_tasks: true,
                last_heartbeat: HashMap::new(),
                health_config_by_agent: HashMap::new(),
                circuit_breakers: HashMap::new(),
                execution_contexts: HashMap::new(),
            })),
        };
        manager.bootstrap_from_store().await?;
        Ok(manager)
    }

    async fn bootstrap_from_store(&self) -> Result<(), NeuromancerError> {
        let recoverable = self
            .store
            .list_tasks_by_states(&["queued", "dispatched", "running"])
            .await?;
        let mut recovered = Vec::with_capacity(recoverable.len());
        for mut task in recoverable {
            let mut enqueue = true;
            if let TaskState::Running { execution_state } = &task.state
                && let TaskExecutionState::Suspended { reason, .. } = execution_state
                && reason == "daemon shutdown"
            {
                let has_checkpoint = match self.store.latest_checkpoint(task.id).await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(err) => {
                        tracing::warn!(
                            task_id = %task.id,
                            error = ?err,
                            "task_checkpoint_load_failed_during_recovery"
                        );
                        false
                    }
                };
                if has_checkpoint {
                    let from_state = task_state_label(&task.state).to_string();
                    task.state = TaskState::Queued;
                    task.updated_at = Utc::now();
                    self.store.upsert_task(&task).await?;
                    self.store
                        .record_transition(
                            task.id,
                            &from_state,
                            "queued",
                            Some(json!({ "reason": "restart_resume" })),
                        )
                        .await?;
                } else {
                    let from_state = task_state_label(&task.state).to_string();
                    task.state = TaskState::Failed {
                        error: AgentErrorLike::new(
                            "checkpoint_corrupted",
                            format!(
                                "missing or corrupted checkpoint for suspended task '{}'",
                                task.id
                            ),
                        ),
                    };
                    task.updated_at = Utc::now();
                    self.store.upsert_task(&task).await?;
                    self.store
                        .record_transition(
                            task.id,
                            &from_state,
                            "failed",
                            Some(json!({ "reason": "checkpoint_corrupted" })),
                        )
                        .await?;
                    enqueue = false;
                }
            }
            recovered.push((task, enqueue));
        }

        let mut state = self.state.lock().await;
        for (task, enqueue) in recovered {
            let task_id = task.id;
            state.tasks.insert(task_id, task.clone());
            if enqueue && state.queued_ids.insert(task_id) {
                state.queue.push(QueueItem {
                    task_id,
                    priority: task.priority as i64,
                    created_at: task.created_at,
                });
            }
        }
        if !state.queue.is_empty() {
            self.notify.notify_waiters();
        }
        Ok(())
    }

    pub(crate) async fn register_agent_health(&self, agent_id: String, health: AgentHealthConfig) {
        let mut state = self.state.lock().await;
        if let Some(config) = health.circuit_breaker.clone() {
            state
                .circuit_breakers
                .entry(agent_id.clone())
                .or_insert_with(|| CircuitBreakerState::new(config));
        }
        state.health_config_by_agent.insert(agent_id, health);
    }

    pub(crate) async fn enqueue_direct(&self, mut task: Task) -> Result<TaskId, NeuromancerError> {
        let idempotency_key = task.idempotency_key.clone();

        {
            let state = self.state.lock().await;
            if !state.accepting_new_tasks {
                return Err(NeuromancerError::Infra(InfraError::Config(
                    "task manager is not accepting new tasks".to_string(),
                )));
            }

            if let Some(key) = idempotency_key.as_deref()
                && let Some(existing) = state.tasks.values().find(|candidate| {
                    candidate.idempotency_key.as_deref() == Some(key)
                        && !candidate.state.is_terminal()
                })
            {
                return Ok(existing.id);
            }

            if let Some(breaker) = state.circuit_breakers.get(&task.assigned_agent)
                && breaker.is_open()
            {
                return Err(NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: "task.enqueue".to_string(),
                    message: format!("circuit breaker open for agent '{}'", task.assigned_agent),
                }));
            }
        }

        if let Some(key) = idempotency_key.as_deref()
            && let Some(existing) = self.store.find_non_terminal_by_idempotency(key).await?
        {
            return Ok(existing);
        }

        task.state = TaskState::Queued;
        let now = Utc::now();
        task.updated_at = now;
        if task.created_at > task.updated_at {
            task.created_at = now;
        }
        self.store.upsert_task(&task).await?;

        let task_id = task.id;
        {
            let mut state = self.state.lock().await;
            state.tasks.insert(task_id, task.clone());
            if state.queued_ids.insert(task_id) {
                state.queue.push(QueueItem {
                    task_id,
                    priority: task.priority as i64,
                    created_at: task.created_at,
                });
            }
        }
        self.notify.notify_waiters();
        Ok(task_id)
    }

    pub(crate) async fn await_task_result(
        &self,
        task_id: TaskId,
        timeout: Duration,
    ) -> Result<TaskOutput, NeuromancerError> {
        let rx = {
            let mut state = self.state.lock().await;
            if let Some(task) = state.tasks.get(&task_id)
                && let Some(outcome) = terminal_result(&task.state)
            {
                return map_terminal_outcome(outcome);
            }
            let (tx, rx) = oneshot::channel();
            state.waiters.entry(task_id).or_default().push(tx);
            rx
        };

        // Handle already-terminal tasks not present in the in-memory map.
        if let Some(result) = self.resolve_terminal_result(task_id).await? {
            return result;
        }

        let result = tokio::time::timeout(timeout, rx).await.map_err(|_| {
            NeuromancerError::Agent(AgentError::Timeout {
                task_id,
                elapsed: timeout,
            })
        })?;
        let received = result.map_err(|_| {
            NeuromancerError::Infra(InfraError::Config(
                "task result waiter channel closed".to_string(),
            ))
        })?;
        received.map_err(|message| {
            NeuromancerError::Tool(ToolError::ExecutionFailed {
                tool_id: "task.await".to_string(),
                message,
            })
        })
    }

    pub(crate) async fn transition(
        &self,
        task_id: TaskId,
        expected_from: Option<&str>,
        to: TaskState,
        checkpoint: Option<PersistedCheckpoint>,
    ) -> Result<Task, NeuromancerError> {
        let (from_state, updated_task, waiters, checkpoint_to_save, is_failure) = {
            let mut state = self.state.lock().await;
            let (from_state, updated_task, is_running, is_terminal, is_failure) = {
                let Some(task) = state.tasks.get_mut(&task_id) else {
                    return Err(NeuromancerError::Infra(InfraError::Config(format!(
                        "unknown task '{}'",
                        task_id
                    ))));
                };

                let from_state = task_state_label(&task.state).to_string();
                if let Some(expected) = expected_from
                    && from_state != expected
                {
                    return Err(NeuromancerError::Infra(InfraError::Config(format!(
                        "invalid task transition for '{}': expected '{}', got '{}'",
                        task_id, expected, from_state
                    ))));
                }

                task.state = to;
                task.updated_at = Utc::now();

                if let Some(checkpoint) = checkpoint.as_ref() {
                    let state_data =
                        serde_json::to_value(&checkpoint.execution_state).map_err(|err| {
                            NeuromancerError::Infra(InfraError::Database(err.to_string()))
                        })?;
                    task.checkpoints.push(Checkpoint {
                        task_id,
                        state_data,
                        created_at: checkpoint.created_at,
                    });
                }

                (
                    from_state,
                    task.clone(),
                    matches!(task.state, TaskState::Running { .. }),
                    task.state.is_terminal(),
                    matches!(task.state, TaskState::Failed { .. }),
                )
            };

            if is_running {
                state.last_heartbeat.insert(task_id, Utc::now());
            } else {
                state.last_heartbeat.remove(&task_id);
            }

            let waiters = if is_terminal {
                state.waiters.remove(&task_id).unwrap_or_default()
            } else {
                Vec::new()
            };

            (from_state, updated_task, waiters, checkpoint, is_failure)
        };

        self.store.upsert_task(&updated_task).await?;
        self.store
            .record_transition(
                task_id,
                &from_state,
                task_state_label(&updated_task.state),
                None,
            )
            .await?;
        if let Some(checkpoint) = checkpoint_to_save.as_ref() {
            self.store.save_checkpoint(checkpoint).await?;
        }

        if is_failure {
            let mut state = self.state.lock().await;
            if let Some(breaker) = state.circuit_breakers.get_mut(&updated_task.assigned_agent) {
                breaker.record_failure(Utc::now());
            }
        } else if matches!(updated_task.state, TaskState::Completed { .. }) {
            let mut state = self.state.lock().await;
            if let Some(breaker) = state.circuit_breakers.get_mut(&updated_task.assigned_agent) {
                breaker.reset();
            }
        }

        let completion = terminal_result(&updated_task.state);
        for waiter in waiters {
            let _ =
                waiter.send(completion.clone().unwrap_or_else(|| {
                    Err("task transitioned without terminal output".to_string())
                }));
        }

        Ok(updated_task)
    }

    pub(crate) async fn cancel(
        &self,
        task_id: TaskId,
        reason: impl Into<String>,
    ) -> Result<Task, NeuromancerError> {
        self.transition(
            task_id,
            None,
            TaskState::Cancelled {
                reason: reason.into(),
            },
            None,
        )
        .await
    }

    pub(crate) async fn wait_for_next_task(&self) -> Option<Task> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().await;
                if let Some(item) = state.queue.pop() {
                    state.queued_ids.remove(&item.task_id);
                    if let Some(task) = state.tasks.get(&item.task_id) {
                        return Some(task.clone());
                    }
                }
                if !state.accepting_new_tasks && state.queue.is_empty() {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub(crate) async fn queue_snapshot(&self) -> (usize, usize, usize, usize, usize) {
        let state = self.state.lock().await;
        let mut queued = 0usize;
        let mut running = 0usize;
        let mut completed = 0usize;
        let mut failed = 0usize;
        for task in state.tasks.values() {
            match task.state {
                TaskState::Queued | TaskState::Dispatched => queued += 1,
                TaskState::Running { .. } => running += 1,
                TaskState::Completed { .. } => completed += 1,
                TaskState::Failed { .. } | TaskState::Cancelled { .. } => failed += 1,
            }
        }
        (queued, running, completed, failed, state.tasks.len())
    }

    pub(crate) async fn get_task(&self, task_id: TaskId) -> Result<Option<Task>, NeuromancerError> {
        {
            let state = self.state.lock().await;
            if let Some(task) = state.tasks.get(&task_id) {
                return Ok(Some(task.clone()));
            }
        }
        self.store.get_task(task_id).await
    }

    pub(crate) async fn register_execution_context(
        &self,
        task_id: TaskId,
        context: TaskExecutionContext,
    ) {
        let mut state = self.state.lock().await;
        state.execution_contexts.insert(task_id, context);
    }

    pub(crate) async fn execution_context_for(
        &self,
        task_id: TaskId,
    ) -> Option<TaskExecutionContext> {
        let state = self.state.lock().await;
        state.execution_contexts.get(&task_id).cloned()
    }

    pub(crate) async fn stop_accepting_new_tasks(&self) {
        let mut state = self.state.lock().await;
        state.accepting_new_tasks = false;
        self.notify.notify_waiters();
    }

    pub(crate) async fn mark_heartbeat(&self, task_id: TaskId) {
        let mut state = self.state.lock().await;
        let is_running = state
            .tasks
            .get(&task_id)
            .map(|task| matches!(task.state, TaskState::Running { .. }))
            .unwrap_or(false);
        if is_running {
            state.last_heartbeat.insert(task_id, Utc::now());
        }
    }

    pub(crate) async fn stale_tasks_for_watchdog(&self) -> Vec<WatchdogCandidate> {
        let state = self.state.lock().await;
        let now = Utc::now();
        let mut stale = Vec::new();
        for task in state.tasks.values() {
            if !matches!(task.state, TaskState::Running { .. }) {
                continue;
            }
            let Some(last_seen) = state.last_heartbeat.get(&task.id) else {
                continue;
            };
            let Some(health) = state.health_config_by_agent.get(&task.assigned_agent) else {
                continue;
            };
            let allowed_silence = health
                .heartbeat_interval
                .checked_mul(2)
                .unwrap_or(health.heartbeat_interval);
            let elapsed = now
                .signed_duration_since(*last_seen)
                .to_std()
                .unwrap_or_default();
            if elapsed > allowed_silence {
                stale.push(WatchdogCandidate {
                    task_id: task.id,
                    agent_id: task.assigned_agent.clone(),
                    elapsed,
                    watchdog_timeout: health.watchdog_timeout,
                });
            }
        }
        stale
    }

    pub(crate) async fn suspend_non_terminal_for_shutdown(
        &self,
        reason: &str,
    ) -> Result<usize, NeuromancerError> {
        let candidates = {
            let state = self.state.lock().await;
            state
                .tasks
                .values()
                .filter(|task| !task.state.is_terminal())
                .map(|task| (task.id, task_state_label(&task.state).to_string()))
                .collect::<Vec<_>>()
        };

        let mut suspended = 0usize;
        for (task_id, from_state) in candidates {
            let checkpoint = shutdown_suspend_checkpoint(task_id, reason);
            match self
                .transition(
                    task_id,
                    Some(&from_state),
                    running_state(checkpoint.execution_state.clone()),
                    Some(checkpoint),
                )
                .await
            {
                Ok(_) => suspended += 1,
                Err(err) => {
                    if !is_transition_race(&err) {
                        return Err(err);
                    }
                }
            }
        }

        Ok(suspended)
    }

    pub(crate) async fn is_circuit_open(&self, agent_id: &str) -> bool {
        let state = self.state.lock().await;
        state
            .circuit_breakers
            .get(agent_id)
            .map(CircuitBreakerState::is_open)
            .unwrap_or(false)
    }

    async fn resolve_terminal_result(
        &self,
        task_id: TaskId,
    ) -> Result<Option<Result<TaskOutput, NeuromancerError>>, NeuromancerError> {
        if let Some(task) = {
            let state = self.state.lock().await;
            state.tasks.get(&task_id).cloned()
        } {
            if let Some(outcome) = terminal_result(&task.state) {
                return Ok(Some(outcome.map_err(|message| {
                    NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "task.await".to_string(),
                        message,
                    })
                })));
            }
        }

        let from_store = self.store.get_task(task_id).await?;
        Ok(from_store.and_then(|task| {
            terminal_result(&task.state).map(|outcome| {
                outcome.map_err(|message| {
                    NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "task.await".to_string(),
                        message,
                    })
                })
            })
        }))
    }
}

fn terminal_result(state: &TaskState) -> Option<Result<TaskOutput, String>> {
    match state {
        TaskState::Completed { output } => Some(Ok(output.clone())),
        TaskState::Failed { error } => Some(Err(format!("{}: {}", error.code, error.message))),
        TaskState::Cancelled { reason } => Some(Err(format!("cancelled: {reason}"))),
        _ => None,
    }
}

fn map_terminal_outcome(
    outcome: Result<TaskOutput, String>,
) -> Result<TaskOutput, NeuromancerError> {
    outcome.map_err(|message| {
        NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "task.await".to_string(),
            message,
        })
    })
}

pub(crate) fn task_failure_payload(err: impl Into<String>) -> AgentErrorLike {
    AgentErrorLike::new("task_failed", err.into())
}

pub(crate) fn running_state(execution_state: TaskExecutionState) -> TaskState {
    TaskState::Running { execution_state }
}

fn shutdown_suspend_checkpoint(task_id: TaskId, reason: &str) -> PersistedCheckpoint {
    let created_at = Utc::now();
    let marker = Checkpoint {
        task_id,
        state_data: json!({
            "event": "daemon_shutdown_suspend",
            "reason": reason,
        }),
        created_at,
    };
    let execution_state = TaskExecutionState::Suspended {
        checkpoint: marker,
        reason: reason.to_string(),
    };
    PersistedCheckpoint {
        task_id,
        created_at,
        execution_state,
        conversation_json: None,
        reason: Some(reason.to_string()),
    }
}

fn is_transition_race(err: &NeuromancerError) -> bool {
    matches!(
        err,
        NeuromancerError::Infra(InfraError::Config(message))
            if message.contains("invalid task transition")
                || message.contains("unknown task")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use neuromancer_core::agent::{CircuitBreakerConfig, TaskExecutionState};
    use neuromancer_core::task::{TaskPriority, TokenUsage};
    use neuromancer_core::trigger::TriggerSource;

    fn test_task(priority: TaskPriority, instruction: &str) -> Task {
        let now = Utc::now();
        Task {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            trigger_source: TriggerSource::Internal,
            instruction: instruction.to_string(),
            assigned_agent: "planner".to_string(),
            state: TaskState::Queued,
            priority,
            deadline: None,
            checkpoints: vec![],
            idempotency_key: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn queue_orders_by_priority_then_fifo() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        let low = test_task(TaskPriority::Low, "low");
        let high = test_task(TaskPriority::High, "high");
        manager
            .enqueue_direct(low.clone())
            .await
            .expect("enqueue low");
        manager
            .enqueue_direct(high.clone())
            .await
            .expect("enqueue high");

        let first = manager.wait_for_next_task().await.expect("first");
        assert_eq!(first.id, high.id);
        let second = manager.wait_for_next_task().await.expect("second");
        assert_eq!(second.id, low.id);
    }

    #[tokio::test]
    async fn await_returns_immediately_for_terminal_task() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        let task = test_task(TaskPriority::Normal, "done");
        let task_id = manager.enqueue_direct(task).await.expect("enqueue");
        manager
            .transition(
                task_id,
                Some("queued"),
                TaskState::Completed {
                    output: TaskOutput {
                        artifacts: vec![],
                        summary: "ok".to_string(),
                        token_usage: TokenUsage::default(),
                        duration: Duration::from_millis(1),
                    },
                },
                None,
            )
            .await
            .expect("complete");

        let output = manager
            .await_task_result(task_id, Duration::from_secs(1))
            .await
            .expect("await");
        assert_eq!(output.summary, "ok");
    }

    #[tokio::test]
    async fn await_times_out_when_not_terminal() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        let task_id = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "slow"))
            .await
            .expect("enqueue");

        let err = manager
            .await_task_result(task_id, Duration::from_millis(25))
            .await
            .expect_err("timeout expected");
        assert!(matches!(
            err,
            NeuromancerError::Agent(AgentError::Timeout { .. })
        ));
    }

    #[tokio::test]
    async fn cancel_notifies_waiters() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        let task_id = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "cancel me"))
            .await
            .expect("enqueue");

        let manager_for_wait = manager.clone();
        let waiter = tokio::spawn(async move {
            manager_for_wait
                .await_task_result(task_id, Duration::from_secs(1))
                .await
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        manager
            .cancel(task_id, "user requested")
            .await
            .expect("cancel");
        let err = waiter
            .await
            .expect("join")
            .expect_err("cancel should error");
        assert!(matches!(
            err,
            NeuromancerError::Tool(ToolError::ExecutionFailed { .. })
        ));
    }

    #[tokio::test]
    async fn multiple_waiters_receive_same_terminal_output() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        let task_id = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "fanout"))
            .await
            .expect("enqueue");

        let manager_a = manager.clone();
        let waiter_a = tokio::spawn(async move {
            manager_a
                .await_task_result(task_id, Duration::from_secs(1))
                .await
        });
        let manager_b = manager.clone();
        let waiter_b = tokio::spawn(async move {
            manager_b
                .await_task_result(task_id, Duration::from_secs(1))
                .await
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        manager
            .transition(
                task_id,
                Some("queued"),
                TaskState::Completed {
                    output: TaskOutput {
                        artifacts: vec![],
                        summary: "fanout-ok".to_string(),
                        token_usage: TokenUsage::default(),
                        duration: Duration::from_millis(1),
                    },
                },
                None,
            )
            .await
            .expect("complete");

        let out_a = waiter_a.await.expect("join a").expect("result a");
        let out_b = waiter_b.await.expect("join b").expect("result b");
        assert_eq!(out_a.summary, "fanout-ok");
        assert_eq!(out_b.summary, "fanout-ok");
    }

    #[tokio::test]
    async fn stale_watchdog_candidates_are_reported() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        manager
            .register_agent_health(
                "planner".to_string(),
                AgentHealthConfig {
                    heartbeat_interval: Duration::from_millis(15),
                    watchdog_timeout: Duration::from_millis(30),
                    circuit_breaker: None,
                },
            )
            .await;

        let task_id = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "watchdog"))
            .await
            .expect("enqueue");
        manager
            .transition(
                task_id,
                Some("queued"),
                running_state(TaskExecutionState::Thinking {
                    conversation_len: 0,
                    iteration: 0,
                }),
                None,
            )
            .await
            .expect("running");
        manager.mark_heartbeat(task_id).await;
        tokio::time::sleep(Duration::from_millis(40)).await;

        let stale = manager.stale_tasks_for_watchdog().await;
        assert!(stale.iter().any(|candidate| {
            candidate.task_id == task_id
                && candidate.agent_id == "planner"
                && candidate.watchdog_timeout == Duration::from_millis(30)
        }));
    }

    #[tokio::test]
    async fn circuit_breaker_blocks_dispatch_after_threshold() {
        let store = TaskStore::in_memory().await.expect("store");
        let manager = TaskManager::new(store).await.expect("manager");
        manager
            .register_agent_health(
                "planner".to_string(),
                AgentHealthConfig {
                    heartbeat_interval: Duration::from_secs(1),
                    watchdog_timeout: Duration::from_secs(2),
                    circuit_breaker: Some(CircuitBreakerConfig {
                        failures: 2,
                        window: Duration::from_secs(60),
                    }),
                },
            )
            .await;

        let task_a = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "fail-a"))
            .await
            .expect("enqueue a");
        let task_b = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "fail-b"))
            .await
            .expect("enqueue b");
        manager
            .transition(
                task_a,
                Some("queued"),
                TaskState::Failed {
                    error: AgentErrorLike::new("failed", "a"),
                },
                None,
            )
            .await
            .expect("fail a");
        manager
            .transition(
                task_b,
                Some("queued"),
                TaskState::Failed {
                    error: AgentErrorLike::new("failed", "b"),
                },
                None,
            )
            .await
            .expect("fail b");
        assert!(manager.is_circuit_open("planner").await);

        let err = manager
            .enqueue_direct(test_task(TaskPriority::Normal, "blocked"))
            .await
            .expect_err("should be blocked by circuit breaker");
        assert!(matches!(
            err,
            NeuromancerError::Tool(ToolError::ExecutionFailed { .. })
        ));
    }
}
