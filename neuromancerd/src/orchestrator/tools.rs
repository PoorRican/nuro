pub fn default_system0_tools() -> Vec<String> {
    vec![
        "delegate_to_agent".to_string(),
        "list_agents".to_string(),
        "read_config".to_string(),
        "list_proposals".to_string(),
        "get_proposal".to_string(),
        "propose_config_change".to_string(),
        "propose_skill_add".to_string(),
        "propose_skill_update".to_string(),
        "propose_agent_add".to_string(),
        "propose_agent_update".to_string(),
        "authorize_proposal".to_string(),
        "apply_authorized_proposal".to_string(),
        "analyze_failures".to_string(),
        "score_skills".to_string(),
        "adapt_routing".to_string(),
        "record_lesson".to_string(),
        "run_redteam_eval".to_string(),
        "list_audit_records".to_string(),
        "modify_skill".to_string(),
    ]
}

pub fn effective_system0_tool_allowlist(configured: &[String]) -> Vec<String> {
    if configured.is_empty() {
        default_system0_tools()
    } else {
        configured.to_vec()
    }
}
