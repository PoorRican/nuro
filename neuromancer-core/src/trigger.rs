use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

/// Identity of the entity that produced a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Actor {
    User { actor_id: String },
    Service { service_id: String },
    CronJob { job_id: String },
    A2aPeer { agent_id: AgentId },
}

/// Privilege level attached to the trigger event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    User,
    Admin,
    Internal,
    UserConversation,
}

/// Source of a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Chat,
    Cron,
    A2a,
    Cli,
    Internal,
}

/// Payload of a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerPayload {
    Message {
        text: String,
    },
    CronFire {
        job_id: String,
        rendered_instruction: String,
        parameters: serde_json::Value,
    },
    A2aRequest {
        from_agent: AgentId,
        content: serde_json::Value,
    },
}

/// Metadata about the trigger context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TriggerMetadata {
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
    pub thread_id: Option<String>,
    pub message_id: Option<String>,
}

/// A trigger event produced by any trigger source and consumed by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEvent {
    pub trigger_id: String,
    pub occurred_at: DateTime<Utc>,
    pub actor: Actor,
    pub trigger_type: TriggerType,
    pub source: TriggerSource,
    pub payload: TriggerPayload,
    pub metadata: TriggerMetadata,
}
