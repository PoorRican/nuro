use std::fs;
use std::path::Path;

use neuromancer_core::xdg::validate_markdown_prompt_file;

use crate::orchestrator::error::OrchestratorRuntimeError;

pub fn load_system_prompt_file(path: &Path) -> Result<String, OrchestratorRuntimeError> {
    validate_markdown_prompt_file(path)
        .map_err(|err| OrchestratorRuntimeError::Config(err.to_string()))?;
    fs::read_to_string(path).map_err(|err| {
        OrchestratorRuntimeError::Config(format!(
            "failed to read system prompt '{}': {err}",
            path.display()
        ))
    })
}

pub fn render_orchestrator_prompt(
    template: &str,
    mut agents: Vec<String>,
    mut tools: Vec<String>,
) -> String {
    agents.sort();
    tools.sort();
    let rendered_agents = if agents.is_empty() {
        "none".to_string()
    } else {
        agents.join(", ")
    };
    let rendered_tools = if tools.is_empty() {
        "none".to_string()
    } else {
        tools.join(", ")
    };
    template
        .replace("{{ORCHESTRATOR_ID}}", "system0")
        .replace("{{AVAILABLE_AGENTS}}", &rendered_agents)
        .replace("{{AVAILABLE_TOOLS}}", &rendered_tools)
}
