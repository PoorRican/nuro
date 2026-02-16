use neuromancer_core::agent::{AgentConfig, AgentHealthConfig, AgentMode, AgentModelConfig};
use neuromancer_core::config::NeuromancerConfig;

use crate::orchestrator::error::System0Error;
use crate::orchestrator::state::SYSTEM0_AGENT_ID;

pub(crate) fn build_system0_agent_config(
    config: &NeuromancerConfig,
    allowlisted_tools: Vec<String>,
    system_prompt: String,
) -> AgentConfig {
    let mut capabilities = config.orchestrator.capabilities.clone();
    capabilities.skills = allowlisted_tools;
    AgentConfig {
        id: SYSTEM0_AGENT_ID.to_string(),
        mode: AgentMode::Inproc,
        image: None,
        models: AgentModelConfig {
            planner: None,
            executor: config.orchestrator.model_slot.clone(),
            verifier: None,
        },
        capabilities,
        health: AgentHealthConfig::default(),
        system_prompt,
        max_iterations: config.orchestrator.max_iterations,
    }
}

pub(crate) fn map_xdg_err(err: neuromancer_core::xdg::XdgError) -> System0Error {
    System0Error::Config(err.to_string())
}
