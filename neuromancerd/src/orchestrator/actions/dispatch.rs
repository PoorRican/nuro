use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::{ToolCall, ToolResult};

use crate::orchestrator::state::System0ToolBroker;
use crate::orchestrator::tool_id::System0ToolId;

pub(crate) async fn dispatch_tool(
    broker: &System0ToolBroker,
    call: ToolCall,
) -> Result<ToolResult, NeuromancerError> {
    match System0ToolId::try_from(call.tool_id.as_str()) {
        Ok(System0ToolId::Runtime(id)) => broker.handle_runtime_action(id, call).await,
        Ok(System0ToolId::Adaptive(id)) => broker.handle_adaptive_action(id, call).await,
        Ok(System0ToolId::AuthenticatedAdaptive(id)) => {
            broker.handle_authenticated_adaptive_action(id, call).await
        }
        Err(()) => {
            let mut inner = broker.inner.lock().await;
            let turn_id = inner.turn.current_turn_id;
            let err = System0ToolBroker::not_found_err(&call.tool_id);
            inner.runs.record_invocation_err(turn_id, &call, &err);
            Err(err)
        }
    }
}
