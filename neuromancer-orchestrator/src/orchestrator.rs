use tokio::sync::mpsc;

use neuromancer_core::agent::{RemediationAction, SubAgentReport};
use neuromancer_core::error::NeuromancerError;
use neuromancer_core::task::{Task, TaskId, TaskState};
use neuromancer_core::trigger::{TriggerEvent, TriggerPayload};

use crate::registry::AgentRegistry;
use crate::remediation::{self, RemediationPolicy};
use crate::router::Router;
use crate::task_queue::TaskQueue;

/// The main orchestrator that supervises agents and coordinates task execution.
pub struct Orchestrator {
    pub router: Router,
    pub task_queue: TaskQueue,
    pub registry: AgentRegistry,
    pub remediation_policy: RemediationPolicy,
}

impl Orchestrator {
    pub fn new(
        router: Router,
        registry: AgentRegistry,
        remediation_policy: RemediationPolicy,
    ) -> Self {
        Self {
            router,
            task_queue: TaskQueue::new(),
            registry,
            remediation_policy,
        }
    }

    /// Route a trigger event: determine the target agent and create a task.
    pub async fn route(&mut self, event: TriggerEvent) -> Result<TaskId, NeuromancerError> {
        let agent_id = self.router.resolve(&event, &self.registry).await?;

        let instruction = match &event.payload {
            TriggerPayload::Message { text } => text.clone(),
            TriggerPayload::CronFire {
                rendered_instruction,
                ..
            } => rendered_instruction.clone(),
            TriggerPayload::A2aRequest { content, .. } => content.to_string(),
        };

        let task = Task::new(event.source, instruction, agent_id);
        let task_id = self.task_queue.enqueue(task);

        tracing::info!(task_id = %task_id, "task enqueued");
        Ok(task_id)
    }

    /// Handle a sub-agent report, applying remediation logic.
    pub async fn handle_report(&mut self, report: SubAgentReport) {
        let task_id = extract_task_id(&report);

        match &report {
            SubAgentReport::Completed { summary, .. } => {
                tracing::info!(task_id = %task_id, summary = %summary, "task completed");
                self.task_queue.update_state(task_id, TaskState::Completed);

                // Extract agent_id from the task and clear current task
                if let Some(task) = self.task_queue.get(&task_id) {
                    let agent_id = task.assigned_agent.clone();
                    self.registry.set_current_task(&agent_id, None);
                    self.registry.record_heartbeat(&agent_id);
                }
            }

            SubAgentReport::Failed { error, .. } => {
                tracing::error!(task_id = %task_id, error = %error, "task failed");
                self.task_queue.update_state(task_id, TaskState::Failed);

                if let Some(task) = self.task_queue.get(&task_id) {
                    let agent_id = task.assigned_agent.clone();
                    self.registry.set_current_task(&agent_id, None);
                    self.registry.record_failure(&agent_id);
                }
            }

            SubAgentReport::Progress { .. } => {
                if let Some(task) = self.task_queue.get(&task_id) {
                    let agent_id = task.assigned_agent.clone();
                    self.registry.record_heartbeat(&agent_id);
                }
            }

            _ => {
                // Apply remediation
                let action = remediation::remediate(&report, &self.remediation_policy);
                tracing::info!(
                    task_id = %task_id,
                    action = ?action,
                    "applying remediation"
                );
                self.apply_remediation(task_id, action).await;
            }
        }
    }

    /// Apply a remediation action.
    async fn apply_remediation(&mut self, task_id: TaskId, action: RemediationAction) {
        match action {
            RemediationAction::Retry { .. } => {
                if self.task_queue.requeue(task_id) {
                    tracing::info!(task_id = %task_id, "re-enqueuing task for retry");
                } else {
                    tracing::warn!(task_id = %task_id, "retry requested but task was not re-queued");
                }
            }

            RemediationAction::Abort { reason } => {
                tracing::warn!(task_id = %task_id, reason = %reason, "aborting task");
                self.task_queue.update_state(task_id, TaskState::Failed);
                if let Some(task) = self.task_queue.get(&task_id) {
                    let agent_id = task.assigned_agent.clone();
                    self.registry.set_current_task(&agent_id, None);
                }
            }

            RemediationAction::Reassign {
                new_agent_id,
                reason,
            } => {
                tracing::info!(
                    task_id = %task_id,
                    new_agent = %new_agent_id,
                    reason = %reason,
                    "reassigning task"
                );
                let mut reassigned = false;
                if let Some(task) = self.task_queue.get_mut(&task_id) {
                    let old_agent = task.assigned_agent.clone();
                    task.assigned_agent = new_agent_id;
                    task.state = TaskState::Queued;
                    self.registry.set_current_task(&old_agent, None);
                    reassigned = true;
                }
                if reassigned && !self.task_queue.requeue(task_id) {
                    tracing::warn!(task_id = %task_id, "reassign requested but task was not re-queued");
                }
            }

            RemediationAction::EscalateToUser { question, channel } => {
                tracing::info!(
                    task_id = %task_id,
                    channel = %channel,
                    "escalating to user: {question}"
                );
                // In production, this would send a message via the trigger channel.
                // For now, log and suspend the task.
                // TODO: [low pri] implement escalation
                self.task_queue.update_state(task_id, TaskState::Suspended);
            }

            RemediationAction::GrantTemporary {
                capability,
                scope: _,
                ..
            } => {
                tracing::info!(
                    task_id = %task_id,
                    capability = %capability,
                    "granting temporary capability"
                );
                // In production, this would issue a scoped grant.
                // TODO: [low pri] Re-enqueue to retry with the grant.
                if !self.task_queue.requeue(task_id) {
                    tracing::warn!(task_id = %task_id, "temporary grant requested but task was not re-queued");
                }
            }

            RemediationAction::Clarify { additional_context } => {
                tracing::info!(
                    task_id = %task_id,
                    "injecting clarification context"
                );
                // Re-enqueue with additional context appended to instruction
                let mut clarified = false;
                if let Some(task) = self.task_queue.get_mut(&task_id) {
                    task.instruction
                        .push_str(&format!("\n\nAdditional context: {additional_context}"));
                    task.state = TaskState::Queued;
                    clarified = true;
                }
                if clarified && !self.task_queue.requeue(task_id) {
                    tracing::warn!(task_id = %task_id, "clarify requested but task was not re-queued");
                }
            }
        }
    }

    /// Main supervision loop. Processes trigger events, dispatches tasks, and handles reports.
    ///
    /// This is the core event loop from SDS section 6.2.
    pub async fn supervise(
        &mut self,
        mut trigger_rx: mpsc::Receiver<TriggerEvent>,
        mut report_rx: mpsc::Receiver<SubAgentReport>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        tracing::info!("orchestrator supervision loop starting");

        loop {
            // Try to dispatch queued tasks first
            self.try_dispatch().await;

            tokio::select! {
                Some(event) = trigger_rx.recv() => {
                    match self.route(event).await {
                        Ok(task_id) => {
                            tracing::debug!(task_id = %task_id, "trigger routed to task");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to route trigger event");
                        }
                    }
                }

                Some(report) = report_rx.recv() => {
                    self.handle_report(report).await;
                }

                Ok(()) = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::info!("orchestrator received shutdown signal");
                        break;
                    }
                }
            }
        }

        tracing::info!("orchestrator supervision loop stopped");
    }

    /// Try to dispatch queued tasks to agents.
    async fn try_dispatch(&mut self) {
        while let Some(task) = self.task_queue.dequeue() {
            let agent_id = task.assigned_agent.clone();
            let task_id = task.id;

            match self.registry.dispatch(&agent_id, task).await {
                Ok(()) => {
                    tracing::debug!(
                        task_id = %task_id,
                        agent = %agent_id,
                        "task dispatched"
                    );
                    self.task_queue.update_state(task_id, TaskState::Dispatched);
                    self.registry.set_current_task(&agent_id, Some(task_id));
                }
                Err(e) => {
                    tracing::error!(
                        task_id = %task_id,
                        agent = %agent_id,
                        error = %e,
                        "failed to dispatch task"
                    );
                    self.task_queue.update_state(task_id, TaskState::Failed);
                }
            }
        }
    }
}

fn extract_task_id(report: &SubAgentReport) -> TaskId {
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

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::agent::*;
    use neuromancer_core::routing::*;
    use neuromancer_core::trigger::*;

    fn setup() -> Orchestrator {
        let routing_config = RoutingConfig {
            default_agent: "planner".into(),
            classifier_model: None,
            rules: vec![RoutingRule {
                match_criteria: RoutingMatch {
                    channel_id: Some("browser-tasks".into()),
                    ..Default::default()
                },
                agent: "browser".into(),
            }],
        };

        let router = Router::new(&routing_config);
        let mut registry = AgentRegistry::new();
        registry.register(
            "planner".into(),
            AgentConfig {
                id: "planner".into(),
                mode: AgentMode::Inproc,
                image: None,
                models: AgentModelConfig::default(),
                capabilities: AgentCapabilities::default(),
                health: AgentHealthConfig::default(),
                system_prompt: "Planner agent".into(),
                max_iterations: 20,
            },
        );
        registry.register(
            "browser".into(),
            AgentConfig {
                id: "browser".into(),
                mode: AgentMode::Inproc,
                image: None,
                models: AgentModelConfig::default(),
                capabilities: AgentCapabilities::default(),
                health: AgentHealthConfig::default(),
                system_prompt: "Browser agent".into(),
                max_iterations: 20,
            },
        );

        Orchestrator::new(router, registry, RemediationPolicy::default())
    }

    fn make_event(channel: &str, text: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_id: "test".into(),
            occurred_at: chrono::Utc::now(),
            actor: Actor::User {
                actor_id: "u1".into(),
            },
            trigger_type: TriggerType::User,
            source: TriggerSource::Chat,
            payload: TriggerPayload::Message { text: text.into() },
            route_hint: None,
            metadata: TriggerMetadata {
                channel_id: Some(channel.into()),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn route_creates_task() {
        let mut orch = setup();
        let event = make_event("general", "do something");
        let task_id = orch.route(event).await.unwrap();
        assert_eq!(orch.task_queue.total_tasks(), 1);
        let task = orch.task_queue.get(&task_id).unwrap();
        assert_eq!(task.assigned_agent, "planner");
    }

    #[tokio::test]
    async fn route_matches_channel_rule() {
        let mut orch = setup();
        let event = make_event("browser-tasks", "go to website");
        let task_id = orch.route(event).await.unwrap();
        let task = orch.task_queue.get(&task_id).unwrap();
        assert_eq!(task.assigned_agent, "browser");
    }

    #[tokio::test]
    async fn handle_completed_report() {
        let mut orch = setup();
        let event = make_event("general", "task");
        let task_id = orch.route(event).await.unwrap();

        let report = SubAgentReport::Completed {
            task_id,
            artifacts: vec![],
            summary: "done".into(),
        };
        orch.handle_report(report).await;

        let task = orch.task_queue.get(&task_id).unwrap();
        assert_eq!(task.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn handle_failed_report() {
        let mut orch = setup();
        let event = make_event("general", "task");
        let task_id = orch.route(event).await.unwrap();

        let report = SubAgentReport::Failed {
            task_id,
            error: "boom".into(),
            partial_result: None,
        };
        orch.handle_report(report).await;

        let task = orch.task_queue.get(&task_id).unwrap();
        assert_eq!(task.state, TaskState::Failed);
    }

    #[tokio::test]
    async fn handle_tool_failure_retries() {
        let mut orch = setup();
        let event = make_event("general", "task");
        let task_id = orch.route(event).await.unwrap();

        // Dequeue so the task is in Dispatched state
        orch.task_queue.dequeue();

        let report = SubAgentReport::ToolFailure {
            task_id,
            tool_id: "search".into(),
            error: "timeout".into(),
            retry_eligible: true,
            attempted_count: 1,
        };
        orch.handle_report(report).await;

        // Task should be re-queued
        let task = orch.task_queue.get(&task_id).unwrap();
        assert_eq!(task.state, TaskState::Queued);
    }

    #[tokio::test]
    async fn retry_path_requeues_task_for_future_dispatch() {
        let mut orch = setup();
        let event = make_event("general", "task");
        let task_id = orch.route(event).await.unwrap();

        // Simulate the orchestrator dispatch loop taking the task.
        let dispatched = orch.task_queue.dequeue().unwrap();
        assert_eq!(dispatched.id, task_id);
        assert_eq!(orch.task_queue.queue_len(), 0);

        let report = SubAgentReport::ToolFailure {
            task_id,
            tool_id: "search".into(),
            error: "timeout".into(),
            retry_eligible: true,
            attempted_count: 1,
        };
        orch.handle_report(report).await;

        // Regression guard: retry should put the task back into the dispatch queue.
        // This currently fails because only task state is updated, without queue insertion.
        assert_eq!(orch.task_queue.queue_len(), 1);
        let retried = orch.task_queue.dequeue().unwrap();
        assert_eq!(retried.id, task_id);
    }
}
