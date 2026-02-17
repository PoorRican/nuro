use std::collections::{HashMap, VecDeque};

use neuromancer_core::error::NeuromancerError;
use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorOutputItem, OrchestratorToolInvocation, TurnDelegation,
};
use neuromancer_core::tool::{ToolCall, ToolOutput};

const OUTPUT_QUEUE_MAX_ITEMS: usize = 1_000;

pub(crate) struct RunTracker {
    pub(crate) runs_index: HashMap<String, DelegatedRun>,
    pub(crate) runs_order: Vec<String>,
    pub(crate) delegations_by_turn: HashMap<uuid::Uuid, Vec<TurnDelegation>>,
    pub(crate) tool_invocations_by_turn: HashMap<uuid::Uuid, Vec<OrchestratorToolInvocation>>,
    pub(crate) pending_output_queue: VecDeque<OrchestratorOutputItem>,
}

impl RunTracker {
    pub(crate) fn new() -> Self {
        Self {
            runs_index: HashMap::new(),
            runs_order: Vec::new(),
            delegations_by_turn: HashMap::new(),
            tool_invocations_by_turn: HashMap::new(),
            pending_output_queue: VecDeque::new(),
        }
    }

    pub(crate) fn record_invocation(
        &mut self,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        output: &ToolOutput,
    ) {
        let (status, rendered_output) = match output {
            ToolOutput::Success(value) => ("success".to_string(), value.clone()),
            ToolOutput::Error(err) => ("error".to_string(), serde_json::json!({ "error": err })),
        };

        self.tool_invocations_by_turn
            .entry(turn_id)
            .or_default()
            .push(OrchestratorToolInvocation {
                call_id: call.id.clone(),
                tool_id: call.tool_id.clone(),
                arguments: call.arguments.clone(),
                status,
                output: rendered_output,
            });
    }

    pub(crate) fn record_invocation_err(
        &mut self,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        err: &NeuromancerError,
    ) {
        self.record_invocation(turn_id, call, &ToolOutput::Error(err.to_string()));
    }

    pub(crate) fn push_output(&mut self, item: OrchestratorOutputItem) {
        if self.pending_output_queue.len() >= OUTPUT_QUEUE_MAX_ITEMS {
            self.pending_output_queue.pop_front();
        }
        self.pending_output_queue.push_back(item);
    }
}
