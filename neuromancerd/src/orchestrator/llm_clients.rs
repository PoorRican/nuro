use std::sync::Arc;

use neuromancer_agent::llm::{LlmClient, RigLlmClient};
use neuromancer_core::agent::AgentConfig;
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::NeuromancerError;
use rig::client::CompletionClient;

use crate::orchestrator::error::System0Error;

pub fn resolve_tool_call_retry_limit(
    config: &NeuromancerConfig,
    agent_config: &AgentConfig,
) -> u32 {
    let slot_name = agent_config
        .models
        .executor
        .as_deref()
        .unwrap_or("executor");
    config
        .models
        .get(slot_name)
        .map(|slot| slot.tool_call_retry_limit)
        .unwrap_or(1)
}

// TODO: should this be moved to core?
pub fn build_llm_client(
    config: &NeuromancerConfig,
    agent_config: &AgentConfig,
) -> Result<Arc<dyn LlmClient>, System0Error> {
    let slot_name = agent_config
        .models
        .executor
        .as_deref()
        .unwrap_or("executor");
    let Some(slot) = config.models.get(slot_name) else {
        return Ok(Arc::new(EchoLlmClient));
    };

    match slot.provider.as_str() {
        "mock" => Ok(Arc::new(super::mock_llm::TwoStepMockLlmClient::default())),
        provider => {
            let env_var = resolve_api_key_env_var(provider);
            let key = std::env::var(&env_var).map_err(|_| {
                System0Error::Config(format!(
                    "{env_var} is required when using provider='{provider}'"
                ))
            })?;

            let base_url = slot
                .base_url
                .as_deref()
                .or_else(|| default_base_url(provider));

            let client: Result<rig::providers::openai::CompletionsClient, _> = if let Some(url) = base_url {
                rig::providers::openai::CompletionsClient::builder()
                    .api_key(&key)
                    .base_url(url)
                    .build()
            } else if provider == "openai" {
                rig::providers::openai::CompletionsClient::new(&key)
            } else {
                return Err(System0Error::Config(format!(
                    "provider '{provider}' requires a base_url in config"
                )));
            };

            let client = client.map_err(|e| {
                System0Error::Config(format!("failed to create LLM client: {e}"))
            })?;

            Ok(Arc::new(RigLlmClient::new(
                client.completion_model(&slot.model),
            )))
        }
    }
}

fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        // TODO: add more provider support
        "groq" => Some("https://api.groq.com/openai/v1"),
        "fireworks" => Some("https://api.fireworks.ai/inference/v1"),
        "xai" => Some("https://api.x.ai/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        _ => None,
    }
}

fn resolve_api_key_env_var(provider: &str) -> String {
    match provider {
        "openai" => "OPENAI_API_KEY".into(),
        "anthropic" => "ANTHROPIC_API_KEY".into(),
        "groq" => "GROQ_API_KEY".into(),
        "fireworks" => "FIREWORKS_API_KEY".into(),
        "gemini" | "google" => "GEMINI_API_KEY".into(),
        "xai" => "XAI_API_KEY".into(),
        "mistral" => "MISTRAL_API_KEY".into(),
        other => format!("{}_API_KEY", other.to_ascii_uppercase()),
    }
}

// TODO: shouldn't this be in it's own module? This is to send smoke
struct EchoLlmClient;

#[async_trait::async_trait]
impl LlmClient for EchoLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        _tool_definitions: Vec<rig::completion::ToolDefinition>,
        _output_schema: Option<schemars::Schema>,
    ) -> Result<neuromancer_agent::llm::LlmResponse, NeuromancerError> {
        let fallback = messages
            .iter()
            .rev()
            .find_map(|msg| match msg {
                rig::completion::Message::User { content } => content.iter().find_map(|part| {
                    if let rig::message::UserContent::Text(text) = part {
                        Some(text.text.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .unwrap_or_else(|| "No message provided.".to_string());

        Ok(neuromancer_agent::llm::LlmResponse {
            text: Some(format!("Echo: {fallback}")),
            tool_calls: vec![],
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }
}
