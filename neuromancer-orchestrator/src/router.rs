use std::sync::Arc;

use neuromancer_core::agent::AgentId;
use neuromancer_core::error::NeuromancerError;
use neuromancer_core::routing::{RoutingConfig, RoutingRule};
use neuromancer_core::trigger::TriggerEvent;

use crate::registry::AgentRegistry;

/// Extracts message text from a trigger event payload for LLM classification.
fn extract_message_text(event: &TriggerEvent) -> Option<String> {
    match &event.payload {
        neuromancer_core::trigger::TriggerPayload::Message { text } => Some(text.clone()),
        neuromancer_core::trigger::TriggerPayload::CronFire {
            rendered_instruction,
            ..
        } => Some(rendered_instruction.clone()),
        neuromancer_core::trigger::TriggerPayload::A2aRequest { content, .. } => {
            Some(content.to_string())
        }
    }
}

/// Router: deterministic rules first, LLM classifier fallback, then default_agent.
pub struct Router {
    rules: Vec<RoutingRule>,
    default_agent: AgentId,
    classifier_model_slot: Option<String>,
    classifier: Option<Arc<dyn LlmClassifier>>,
}

/// Trait for the LLM-based intent classifier used as routing fallback.
#[async_trait::async_trait]
pub trait LlmClassifier: Send + Sync {
    /// Given a message and a list of (agent_id, description) pairs,
    /// return the best matching agent_id.
    async fn classify(
        &self,
        message: &str,
        agents: &[(AgentId, String)],
    ) -> Result<Option<AgentId>, NeuromancerError>;
}

impl Router {
    pub fn new(config: &RoutingConfig) -> Self {
        Self {
            rules: config.rules.clone(),
            default_agent: config.default_agent.clone(),
            classifier_model_slot: config.classifier_model.clone(),
            classifier: None,
        }
    }

    /// Set the LLM classifier for fallback routing.
    pub fn with_classifier(mut self, classifier: Arc<dyn LlmClassifier>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Resolve a trigger event to a target agent.
    ///
    /// 1. Evaluate deterministic rules top-to-bottom; first match wins.
    /// 2. If no rule matches and an LLM classifier is configured, call it.
    /// 3. If still unresolved and route_hint is valid, use it as a suggestion.
    /// 4. Fall back to default_agent.
    pub async fn resolve(
        &self,
        event: &TriggerEvent,
        registry: &AgentRegistry,
    ) -> Result<AgentId, NeuromancerError> {
        // Deterministic rules
        for rule in &self.rules {
            if rule.match_criteria.matches(event) {
                tracing::debug!(
                    agent = %rule.agent,
                    "matched routing rule"
                );
                return Ok(rule.agent.clone());
            }
        }

        // LLM classifier fallback
        if let (Some(classifier), Some(text)) = (&self.classifier, extract_message_text(event)) {
            let agent_descriptions = registry.agent_descriptions();
            match classifier.classify(&text, &agent_descriptions).await {
                Ok(Some(agent_id)) => {
                    tracing::debug!(agent = %agent_id, "LLM classifier resolved");
                    return Ok(agent_id);
                }
                Ok(None) => {
                    tracing::debug!("LLM classifier returned no match, using default");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "LLM classifier failed, using default");
                }
            }
        }

        if let Some(hint) = &event.route_hint {
            if registry.contains(hint) {
                tracing::debug!(agent = %hint, "using route hint as fallback suggestion");
                return Ok(hint.clone());
            }
            tracing::warn!(agent = %hint, "ignoring unknown route hint");
        }

        // Default fallback
        tracing::debug!(agent = %self.default_agent, "using default agent");
        Ok(self.default_agent.clone())
    }

    pub fn default_agent(&self) -> &AgentId {
        &self.default_agent
    }

    pub fn classifier_model_slot(&self) -> Option<&str> {
        self.classifier_model_slot.as_deref()
    }
}

/// Simple LLM classifier that uses a rig CompletionModel.
pub struct RigLlmClassifier<M: rig::completion::CompletionModel> {
    model: M,
}

impl<M: rig::completion::CompletionModel> RigLlmClassifier<M> {
    pub fn new(model: M) -> Self {
        Self { model }
    }
}

#[async_trait::async_trait]
impl<M> LlmClassifier for RigLlmClassifier<M>
where
    M: rig::completion::CompletionModel + Send + Sync + 'static,
    M::Response: Send + Sync,
{
    async fn classify(
        &self,
        message: &str,
        agents: &[(AgentId, String)],
    ) -> Result<Option<AgentId>, NeuromancerError> {
        let agents_list = agents
            .iter()
            .map(|(id, desc)| format!("- {id}: {desc}"))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Given the following user message, determine which agent should handle it.\n\
             Available agents:\n{agents_list}\n\n\
             User message: \"{message}\"\n\n\
             Respond with ONLY the agent name (e.g., \"planner\"). \
             If unsure, respond with \"unknown\"."
        );

        let request = self
            .model
            .completion_request(prompt.clone())
            .preamble("You are a routing classifier. Respond with only an agent name.".into())
            .build();

        let response = self.model.completion(request).await.map_err(|e| {
            NeuromancerError::Llm(neuromancer_core::error::LlmError::InvalidResponse {
                reason: e.to_string(),
            })
        })?;

        let text = response
            .choice
            .iter()
            .find_map(|c| {
                if let rig::message::AssistantContent::Text(t) = c {
                    Some(t.text.trim().to_lowercase())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if text == "unknown" || text.is_empty() {
            return Ok(None);
        }

        // Check if the response matches a known agent
        if agents.iter().any(|(id, _)| id == &text) {
            Ok(Some(text))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::routing::{RoutingConfig, RoutingMatch, RoutingRule};
    use neuromancer_core::trigger::*;

    fn make_event(channel: &str, text: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_id: "test".into(),
            occurred_at: chrono::Utc::now(),
            actor: Actor::User {
                actor_id: "u1".into(),
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

    fn make_registry() -> AgentRegistry {
        let mut reg = AgentRegistry::new();
        reg.register(
            "browser".into(),
            neuromancer_core::agent::AgentConfig {
                id: "browser".into(),
                mode: neuromancer_core::agent::AgentMode::Inproc,
                image: None,
                models: Default::default(),
                capabilities: Default::default(),
                health: Default::default(),
                preamble: Some("Browser agent".into()),
                max_iterations: 20,
            },
        );
        reg.register(
            "planner".into(),
            neuromancer_core::agent::AgentConfig {
                id: "planner".into(),
                mode: neuromancer_core::agent::AgentMode::Inproc,
                image: None,
                models: Default::default(),
                capabilities: Default::default(),
                health: Default::default(),
                preamble: Some("Planner agent".into()),
                max_iterations: 20,
            },
        );
        reg
    }

    #[tokio::test]
    async fn route_hint_takes_precedence() {
        let config = RoutingConfig {
            default_agent: "planner".into(),
            classifier_model: None,
            rules: vec![],
        };
        let router = Router::new(&config);
        let registry = make_registry();

        let mut event = make_event("general", "do something");
        event.route_hint = Some("browser".into());

        let result = router.resolve(&event, &registry).await.unwrap();
        assert_eq!(result, "browser");
    }

    #[tokio::test]
    async fn unknown_route_hint_falls_back_to_safe_resolution() {
        let config = RoutingConfig {
            default_agent: "planner".into(),
            classifier_model: None,
            rules: vec![],
        };
        let router = Router::new(&config);
        let registry = make_registry();

        let mut event = make_event("general", "do something");
        event.route_hint = Some("ghost-agent".into());

        // Regression guard: route hints should not bypass agent existence checks.
        // Unknown hints should fall back to deterministic/default routing.
        let result = router.resolve(&event, &registry).await.unwrap();
        assert_eq!(result, "planner");
    }

    #[tokio::test]
    async fn deterministic_rule_match() {
        let config = RoutingConfig {
            default_agent: "planner".into(),
            classifier_model: None,
            rules: vec![RoutingRule {
                match_criteria: RoutingMatch {
                    channel_id: Some("browser-tasks".into()),
                    ..Default::default()
                },
                agent: "browser".into(),
            }],
        };
        let router = Router::new(&config);
        let registry = make_registry();

        let event = make_event("browser-tasks", "go to website");
        let result = router.resolve(&event, &registry).await.unwrap();
        assert_eq!(result, "browser");
    }

    #[tokio::test]
    async fn falls_back_to_default() {
        let config = RoutingConfig {
            default_agent: "planner".into(),
            classifier_model: None,
            rules: vec![RoutingRule {
                match_criteria: RoutingMatch {
                    channel_id: Some("browser-tasks".into()),
                    ..Default::default()
                },
                agent: "browser".into(),
            }],
        };
        let router = Router::new(&config);
        let registry = make_registry();

        let event = make_event("general", "do something");
        let result = router.resolve(&event, &registry).await.unwrap();
        assert_eq!(result, "planner");
    }
}
