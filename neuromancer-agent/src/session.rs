use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::conversation::ConversationContext;

pub type AgentSessionId = uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentSessionState {
    pub id: AgentSessionId,
    pub conversation: ConversationContext,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Default)]
pub struct InMemorySessionStore {
    sessions: Arc<RwLock<HashMap<AgentSessionId, AgentSessionState>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load_or_create(
        &self,
        session_id: AgentSessionId,
        default_conversation: ConversationContext,
    ) -> AgentSessionState {
        let mut sessions = self.sessions.write().await;
        sessions
            .entry(session_id)
            .or_insert_with(|| AgentSessionState {
                id: session_id,
                conversation: default_conversation,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .clone()
    }

    pub async fn save_conversation(
        &self,
        session_id: AgentSessionId,
        conversation: ConversationContext,
    ) {
        let mut sessions = self.sessions.write().await;
        let now = Utc::now();
        if let Some(existing) = sessions.get_mut(&session_id) {
            existing.conversation = conversation;
            existing.updated_at = now;
            return;
        }

        sessions.insert(
            session_id,
            AgentSessionState {
                id: session_id,
                conversation,
                created_at: now,
                updated_at: now,
            },
        );
    }

    pub async fn get(&self, session_id: AgentSessionId) -> Option<AgentSessionState> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).cloned()
    }
}
