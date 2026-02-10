use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

/// Identity of the entity that produced a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Principal {
    DiscordUser { user_id: String, guild_id: Option<String> },
    System,
    Cron { job_id: String },
    A2aPeer { agent_id: AgentId },
    Admin,
}

/// Source of a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Discord,
    Cron,
    A2a,
    AdminApi,
    Internal,
}

/// Payload of a trigger event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerPayload {
    Message {
        text: String,
        attachments: Vec<String>,
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
    AdminCommand {
        instruction: String,
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
    pub principal: Principal,
    pub payload: TriggerPayload,
    pub route_hint: Option<AgentId>,
    pub metadata: TriggerMetadata,
}
