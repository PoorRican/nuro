use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::trigger::{Actor, TriggerEvent, TriggerPayload, TriggerSource, TriggerType};

/// A deterministic routing rule: match criteria → target agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    #[serde(rename = "match")]
    pub match_criteria: RoutingMatch,
    pub agent: AgentId,
}

/// Criteria for matching a trigger event to a routing rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingMatch {
    pub source: Option<TriggerSource>,
    pub trigger_type: Option<TriggerType>,
    pub channel_id: Option<String>,
    pub actor_id: Option<String>,
    pub prefix: Option<String>,
}

impl RoutingMatch {
    /// Check if this match criteria applies to the given event.
    pub fn matches(&self, event: &TriggerEvent) -> bool {
        if let Some(source) = &self.source {
            if event.source != *source {
                return false;
            }
        }
        if let Some(trigger_type) = &self.trigger_type {
            if event.trigger_type != *trigger_type {
                return false;
            }
        }
        if let Some(ref ch) = self.channel_id {
            if event.metadata.channel_id.as_deref() != Some(ch.as_str()) {
                return false;
            }
        }
        if let Some(ref actor_id) = self.actor_id {
            let current_actor_id = match &event.actor {
                Actor::User { actor_id } => Some(actor_id.as_str()),
                Actor::Service { service_id } => Some(service_id.as_str()),
                Actor::CronJob { job_id } => Some(job_id.as_str()),
                Actor::A2aPeer { agent_id } => Some(agent_id.as_str()),
            };
            if current_actor_id != Some(actor_id.as_str()) {
                return false;
            }
        }
        if let Some(prefix) = &self.prefix {
            let text = match &event.payload {
                TriggerPayload::Message { text } => text.as_str(),
                _ => "",
            };
            if !text.starts_with(prefix.as_str()) {
                return false;
            }
        }
        true
    }
}

/// Routing configuration: deterministic rules + LLM classifier fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub default_agent: AgentId,
    pub classifier_model: Option<String>,
    pub rules: Vec<RoutingRule>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::*;
    use chrono::Utc;

    fn make_discord_event(channel: &str, text: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_id: "test".into(),
            occurred_at: Utc::now(),
            actor: Actor::User {
                actor_id: "user1".into(),
            },
            trigger_type: TriggerType::User,
            source: TriggerSource::Chat,
            payload: TriggerPayload::Message { text: text.into() },
            route_hint: None,
            metadata: TriggerMetadata {
                channel_id: Some(channel.into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn channel_id_match() {
        let rule = RoutingMatch {
            channel_id: Some("browser-tasks".into()),
            ..Default::default()
        };
        let event = make_discord_event("browser-tasks", "hello");
        assert!(rule.matches(&event));

        let event2 = make_discord_event("general", "hello");
        assert!(!rule.matches(&event2));
    }

    #[test]
    fn prefix_match() {
        let rule = RoutingMatch {
            prefix: Some("!nm".into()),
            ..Default::default()
        };
        let event = make_discord_event("general", "!nm do something");
        assert!(rule.matches(&event));

        let event2 = make_discord_event("general", "hello there");
        assert!(!rule.matches(&event2));
    }
}
