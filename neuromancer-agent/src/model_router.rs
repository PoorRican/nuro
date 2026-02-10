use std::collections::HashMap;

use neuromancer_core::agent::AgentModelConfig;
use neuromancer_core::config::ModelSlotConfig;
use neuromancer_core::error::NeuromancerError;

/// Maps logical model slot names to model configurations.
/// Resolves per-agent overrides vs global defaults.
pub struct ModelRouter {
    /// Global model slot configurations from [models] config section.
    global_slots: HashMap<String, ModelSlotConfig>,
}

impl ModelRouter {
    pub fn new(global_slots: HashMap<String, ModelSlotConfig>) -> Self {
        Self { global_slots }
    }

    /// Resolve a slot name, applying per-agent overrides.
    /// Agent model config maps roles (planner, executor, verifier) to slot names.
    /// The slot name then resolves to a ModelSlotConfig via the global map.
    pub fn resolve(
        &self,
        slot: &str,
    ) -> Result<&ModelSlotConfig, NeuromancerError> {
        self.global_slots.get(slot).ok_or_else(|| {
            NeuromancerError::Infra(neuromancer_core::error::InfraError::Config(
                format!("model slot '{slot}' not found in global config"),
            ))
        })
    }

    /// Resolve the executor model for an agent, considering agent-level overrides.
    /// If the agent specifies a model slot name for the role, resolve that slot.
    /// Otherwise fall back to the role name itself as a slot.
    pub fn resolve_for_agent(
        &self,
        role: &str,
        agent_models: &AgentModelConfig,
    ) -> Result<&ModelSlotConfig, NeuromancerError> {
        let slot_name = match role {
            "planner" => agent_models.planner.as_deref().unwrap_or("planner"),
            "executor" => agent_models.executor.as_deref().unwrap_or("executor"),
            "verifier" => agent_models.verifier.as_deref().unwrap_or("verifier"),
            other => other,
        };
        self.resolve(slot_name)
    }

    /// Check if a slot exists.
    pub fn has_slot(&self, slot: &str) -> bool {
        self.global_slots.contains_key(slot)
    }

    /// List all available slot names.
    pub fn slot_names(&self) -> Vec<&str> {
        self.global_slots.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_router() -> ModelRouter {
        let mut slots = HashMap::new();
        slots.insert(
            "planner".into(),
            ModelSlotConfig {
                provider: "openai".into(),
                model: "gpt-4o".into(),
            },
        );
        slots.insert(
            "executor".into(),
            ModelSlotConfig {
                provider: "openai".into(),
                model: "gpt-4o".into(),
            },
        );
        slots.insert(
            "browser".into(),
            ModelSlotConfig {
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
            },
        );
        ModelRouter::new(slots)
    }

    #[test]
    fn resolve_existing_slot() {
        let router = make_router();
        let cfg = router.resolve("planner").unwrap();
        assert_eq!(cfg.provider, "openai");
        assert_eq!(cfg.model, "gpt-4o");
    }

    #[test]
    fn resolve_missing_slot_errors() {
        let router = make_router();
        assert!(router.resolve("nonexistent").is_err());
    }

    #[test]
    fn resolve_for_agent_with_override() {
        let router = make_router();
        let agent_models = AgentModelConfig {
            planner: Some("planner".into()),
            executor: Some("browser".into()), // override executor to browser slot
            verifier: None,
        };
        let cfg = router.resolve_for_agent("executor", &agent_models).unwrap();
        assert_eq!(cfg.provider, "anthropic");
    }
}
