use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::{ToolCall, ToolResult};

use crate::orchestrator::actions::{
    adaptive_actions, authenticated_adaptive_actions, runtime_actions,
};
use crate::orchestrator::state::System0ToolBroker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Runtime,
    Adaptive,
    AuthenticatedAdaptive,
    Unknown,
}

pub fn classify_tool(tool_id: &str) -> ToolClass {
    if runtime_actions::contains(tool_id) {
        ToolClass::Runtime
    } else if adaptive_actions::contains(tool_id) {
        ToolClass::Adaptive
    } else if authenticated_adaptive_actions::contains(tool_id) {
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
            let turn_id = inner.current_turn_id;
            let err = System0ToolBroker::not_found_err(&call.tool_id);
            System0ToolBroker::record_invocation_err(&mut inner, turn_id, &call, &err);
            Err(err)
        }
    }
}
