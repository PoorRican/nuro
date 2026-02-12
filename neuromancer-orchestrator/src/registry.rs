use std::collections::HashMap;

use chrono::{DateTime, Utc};

use neuromancer_core::agent::{AgentConfig, AgentId, CircuitBreakerState};
use neuromancer_core::task::Task;

/// Health status of a registered agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHealth {
    Healthy,
    Unresponsive,
    CircuitOpen,
}

/// Entry in the agent registry.
pub struct AgentEntry {
    pub config: AgentConfig,
    pub health: AgentHealth,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub current_task: Option<uuid::Uuid>,
    pub circuit_breaker: Option<CircuitBreakerState>,
    /// Channel to dispatch tasks to this agent.
    pub task_tx: Option<tokio::sync::mpsc::Sender<Task>>,
}

/// Registry tracking all registered agents, their configs, health, and dispatch channels.
pub struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// Register an agent with its config.
    pub fn register(&mut self, agent_id: AgentId, config: AgentConfig) {
        let cb = config
            .health
            .circuit_breaker
            .as_ref()
            .map(|cb_cfg| CircuitBreakerState::new(cb_cfg.clone()));

        self.agents.insert(
            agent_id,
            AgentEntry {
                config,
                health: AgentHealth::Healthy,
                last_heartbeat: None,
                current_task: None,
                circuit_breaker: cb,
                task_tx: None,
            },
        );
    }

    /// Set the task dispatch channel for an agent.
    pub fn set_task_sender(&mut self, agent_id: &AgentId, tx: tokio::sync::mpsc::Sender<Task>) {
        if let Some(entry) = self.agents.get_mut(agent_id) {
            entry.task_tx = Some(tx);
        }
    }

    /// Get an agent entry.
    pub fn get(&self, agent_id: &AgentId) -> Option<&AgentEntry> {
        self.agents.get(agent_id)
    }

    /// Get a mutable agent entry.
    pub fn get_mut(&mut self, agent_id: &AgentId) -> Option<&mut AgentEntry> {
        self.agents.get_mut(agent_id)
    }

    /// Check if an agent is registered.
    pub fn contains(&self, agent_id: &AgentId) -> bool {
        self.agents.contains_key(agent_id)
    }

    /// List all agent IDs.
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }

    /// Get (agent_id, description) pairs for LLM classification.
    pub fn agent_descriptions(&self) -> Vec<(AgentId, String)> {
        self.agents
            .iter()
            .map(|(id, entry)| {
                let desc = entry
                    .config
                    .preamble
                    .clone()
                    .unwrap_or_else(|| format!("Agent: {id}"));
                (id.clone(), desc)
            })
            .collect()
    }

    /// Dispatch a task to an agent via its channel.
    // NOTE: errors should be typed
    pub async fn dispatch(&mut self, agent_id: &AgentId, task: Task) -> Result<(), String> {
        let entry = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| format!("agent '{agent_id}' not found"))?;

        // Check circuit breaker
        // NOTE: what opens the circuit_breaker?
        if let Some(cb) = &entry.circuit_breaker {
            if cb.is_open() {
                return Err(format!("agent '{agent_id}' circuit breaker is open"));
            }
        }

        // Check health
        if entry.health == AgentHealth::CircuitOpen {
            return Err(format!("agent '{agent_id}' is unhealthy (circuit open)"));
        }

        if let Some(tx) = &entry.task_tx {
            tx.send(task)
                .await
                .map_err(|e| format!("failed to dispatch to '{agent_id}': {e}"))?;
            Ok(())
        } else {
            Err(format!("agent '{agent_id}' has no task channel"))
        }
    }

    /// Record a heartbeat from an agent.
    pub fn record_heartbeat(&mut self, agent_id: &AgentId) {
        if let Some(entry) = self.agents.get_mut(agent_id) {
            entry.last_heartbeat = Some(Utc::now());
            entry.health = AgentHealth::Healthy;
        }
    }

    /// Record a failure for an agent's circuit breaker.
    pub fn record_failure(&mut self, agent_id: &AgentId) {
        if let Some(entry) = self.agents.get_mut(agent_id) {
            if let Some(cb) = &mut entry.circuit_breaker {
                cb.record_failure(Utc::now());
                if cb.is_open() {
                    entry.health = AgentHealth::CircuitOpen;
                    tracing::warn!(agent_id = %agent_id, "circuit breaker opened");
                }
            }
        }
    }

    /// Reset circuit breaker for an agent.
    pub fn reset_circuit_breaker(&mut self, agent_id: &AgentId) {
        if let Some(entry) = self.agents.get_mut(agent_id) {
            if let Some(cb) = &mut entry.circuit_breaker {
                cb.reset();
            }
            entry.health = AgentHealth::Healthy;
        }
    }

    /// Mark agent as having a current task.
    pub fn set_current_task(&mut self, agent_id: &AgentId, task_id: Option<uuid::Uuid>) {
        if let Some(entry) = self.agents.get_mut(agent_id) {
            entry.current_task = task_id;
        }
    }

    /// Number of registered agents.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::agent::*;

    fn make_config(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            mode: AgentMode::Inproc,
            image: None,
            models: AgentModelConfig::default(),
            capabilities: AgentCapabilities::default(),
            health: AgentHealthConfig::default(),
            preamble: Some(format!("{id} agent")),
            max_iterations: 20,
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = AgentRegistry::new();
        reg.register("browser".into(), make_config("browser"));
        reg.register("planner".into(), make_config("planner"));

        assert!(reg.contains(&"browser".into()));
        assert!(reg.contains(&"planner".into()));
        assert!(!reg.contains(&"nonexistent".into()));
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn agent_descriptions() {
        let mut reg = AgentRegistry::new();
        reg.register("browser".into(), make_config("browser"));
        reg.register("planner".into(), make_config("planner"));

        let descriptions = reg.agent_descriptions();
        assert_eq!(descriptions.len(), 2);
    }

    #[test]
    fn circuit_breaker_opens_on_failures() {
        let mut config = make_config("flaky");
        config.health.circuit_breaker = Some(CircuitBreakerConfig {
            failures: 2,
            window: std::time::Duration::from_secs(600),
        });

        let mut reg = AgentRegistry::new();
        reg.register("flaky".into(), config);

        reg.record_failure(&"flaky".into());
        assert_eq!(
            reg.get(&"flaky".into()).unwrap().health,
            AgentHealth::Healthy
        );

        reg.record_failure(&"flaky".into());
        assert_eq!(
            reg.get(&"flaky".into()).unwrap().health,
            AgentHealth::CircuitOpen
        );

        // Reset
        reg.reset_circuit_breaker(&"flaky".into());
        assert_eq!(
            reg.get(&"flaky".into()).unwrap().health,
            AgentHealth::Healthy
        );
    }
}
