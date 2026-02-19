use crate::orchestrator::tool_id::System0ToolId;

pub fn default_system0_tools() -> Vec<String> {
    System0ToolId::all()
}

pub fn effective_system0_tool_allowlist(configured: &[String]) -> Vec<String> {
    if configured.is_empty() {
        default_system0_tools()
    } else {
        configured.to_vec()
    }
}
