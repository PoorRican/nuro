use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::{ToolCall, ToolResult};

use crate::orchestrator::state::System0ToolBroker;

const RUNTIME_TOOL_IDS: &[&str] = &[
    "delegate_to_agent",
    "list_agents",
    "read_config",
    "queue_status",
];

const ADAPTIVE_TOOL_IDS: &[&str] = &[
    "list_proposals",
    "get_proposal",
    "propose_config_change",
    "propose_skill_add",
    "propose_skill_update",
    "propose_agent_add",
    "propose_agent_update",
    "analyze_failures",
    "score_skills",
    "adapt_routing",
    "record_lesson",
    "run_redteam_eval",
    "list_audit_records",
];

const AUTHENTICATED_ADAPTIVE_TOOL_IDS: &[&str] = &[
    "authorize_proposal",
    "apply_authorized_proposal",
    "modify_skill",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Runtime,
    Adaptive,
    AuthenticatedAdaptive,
    Unknown,
}

pub fn classify_tool(tool_id: &str) -> ToolClass {
    if RUNTIME_TOOL_IDS.contains(&tool_id) {
        ToolClass::Runtime
    } else if ADAPTIVE_TOOL_IDS.contains(&tool_id) {
        ToolClass::Adaptive
    } else if AUTHENTICATED_ADAPTIVE_TOOL_IDS.contains(&tool_id) {
        ToolClass::AuthenticatedAdaptive
    } else {
        ToolClass::Unknown
    }
}

pub fn is_self_improvement_tool(tool_id: &str) -> bool {
    matches!(
        classify_tool(tool_id),
        ToolClass::Adaptive | ToolClass::AuthenticatedAdaptive
    )
}

pub async fn dispatch_tool(
    broker: &System0ToolBroker,
    call: ToolCall,
) -> Result<ToolResult, NeuromancerError> {
    match classify_tool(&call.tool_id) {
        ToolClass::Runtime => broker.handle_runtime_action(call).await,
        ToolClass::Adaptive => broker.handle_adaptive_action(call).await,
        ToolClass::AuthenticatedAdaptive => broker.handle_authenticated_adaptive_action(call).await,
        ToolClass::Unknown => {
            let mut inner = broker.inner.lock().await;
            let turn_id = inner.turn.current_turn_id;
            let err = System0ToolBroker::not_found_err(&call.tool_id);
            inner.runs.record_invocation_err(turn_id, &call, &err);
            Err(err)
        }
    }
}
