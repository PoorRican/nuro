use std::collections::HashMap;

use crate::orchestrator::state::SubAgentThreadState;
use crate::orchestrator::tracing::thread_journal::ThreadJournal;

pub(crate) struct ThreadRegistry {
    pub(crate) thread_states: HashMap<String, SubAgentThreadState>,
    pub(crate) thread_journal: ThreadJournal,
}
