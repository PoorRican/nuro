use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::trigger::TriggerEvent;

/// A deterministic routing rule: match criteria → target agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    #[serde(rename = "match")]
    pub match_criteria: RoutingMatch,
    pub agent: AgentId,
}

/// Criteria for matching a trigger event to a routing rule.
// NOTE: this contains discord-specific data which should not be present
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingMatch {
    pub channel_id: Option<String>,
    pub trigger: Option<String>,
    pub id: Option<String>,
    pub guild_id: Option<String>,
    pub user_id: Option<String>,
    pub prefix: Option<String>,
}

impl RoutingMatch {
    /// Check if this match criteria applies to the given event.
    pub fn matches(&self, event: &TriggerEvent) -> bool {
        if let Some(ref ch) = self.channel_id {
            if event.metadata.channel_id.as_deref() != Some(ch.as_str()) {
                return false;
            }
        }
        if let Some(ref trig) = self.trigger {
            let source_str = match &event.payload {
                crate::trigger::TriggerPayload::Message { .. } => "discord",
                crate::trigger::TriggerPayload::CronFire { .. } => "cron",
                crate::trigger::TriggerPayload::A2aRequest { .. } => "a2a",
                crate::trigger::TriggerPayload::AdminCommand { .. } => "admin",
            };
            if trig != source_str {
                return false;
            }
        }
        if let Some(ref match_id) = self.id {
            if let crate::trigger::TriggerPayload::CronFire { ref job_id, .. } = event.payload {
                if match_id != job_id {
                    return false;
                }
            }
        }
        if let Some(ref gid) = self.guild_id {
            if event.metadata.guild_id.as_deref() != Some(gid.as_str()) {
                return false;
            }
        }
        if let Some(ref prefix) = self.prefix {
            let text = match &event.payload {
                crate::trigger::TriggerPayload::Message { text, .. } => text.as_str(),
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
            principal: Principal::DiscordUser {
                user_id: "user1".into(),
                guild_id: None,
            },
            payload: TriggerPayload::Message {
                text: text.into(),
                attachments: vec![],
            },
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
