use std::collections::HashMap;
use std::sync::Arc;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::InMemorySessionStore;

use crate::orchestrator::security::execution_guard::ExecutionGuard;

pub(crate) struct AgentRegistry {
    pub(crate) subagents: HashMap<String, Arc<AgentRuntime>>,
    pub(crate) session_store: InMemorySessionStore,
    pub(crate) execution_guard: Arc<dyn ExecutionGuard>,
}
