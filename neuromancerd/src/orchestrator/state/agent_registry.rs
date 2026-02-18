use std::collections::HashMap;
use std::sync::Arc;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_core::thread::ThreadStore;

use crate::orchestrator::security::execution_guard::ExecutionGuard;

pub(crate) struct AgentRegistry {
    pub(crate) subagents: HashMap<String, Arc<AgentRuntime>>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) execution_guard: Arc<dyn ExecutionGuard>,
}
