use std::sync::{Arc, Mutex};

use neuromancer_agent::llm::{LlmClient, RigLlmClient};
use neuromancer_core::agent::AgentConfig;
use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::ToolCall;

use crate::orchestrator::error::OrchestratorRuntimeError;

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

pub fn build_llm_client(
    config: &NeuromancerConfig,
    agent_config: &AgentConfig,
) -> Result<Arc<dyn LlmClient>, OrchestratorRuntimeError> {
    let slot_name = agent_config
        .models
        .executor
        .as_deref()
        .unwrap_or("executor");
    let Some(slot) = config.models.get(slot_name) else {
        return Ok(Arc::new(EchoLlmClient));
    };

    // TODO: this should NOT be part of production code
    //  When running a test, the [`build_llm_client`] should simply be overwritten
    //  using build flags / config flags
    match slot.provider.as_str() {
        "mock" => Ok(Arc::new(TwoStepMockLlmClient::default())),
        provider => {
            let env_var = resolve_api_key_env_var(provider);
            let key = std::env::var(&env_var).map_err(|_| {
                OrchestratorRuntimeError::Config(format!(
                    "{env_var} is required when using provider='{provider}'"
                ))
            })?;

            let client = match (slot.base_url.as_deref(), default_base_url(provider)) {
                (Some(url), _) | (None, Some(url)) => {
                    rig::providers::openai::Client::from_url(&key, url)
                }
                (None, None) if provider == "openai" => rig::providers::openai::Client::new(&key),
                _ => {
                    return Err(OrchestratorRuntimeError::Config(format!(
                        "provider '{provider}' requires a base_url in config"
                    )));
                }
            };

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

#[derive(Default)]
struct TwoStepMockLlmClient {
    issued_tools: Mutex<bool>,
}

#[async_trait::async_trait]
impl LlmClient for TwoStepMockLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<neuromancer_agent::llm::LlmResponse, NeuromancerError> {
        let mut issued_tools = self.issued_tools.lock().map_err(|_| {
            NeuromancerError::Infra(neuromancer_core::error::InfraError::Config(
                "mock llm lock poisoned".to_string(),
            ))
        })?;

        if !*issued_tools && !tool_definitions.is_empty() {
            *issued_tools = true;
            let has_delegate_tool = tool_definitions
                .iter()
                .any(|tool| tool.name == "delegate_to_agent");
            let calls = if has_delegate_tool {
                vec![mock_delegate_call(&messages)]
            } else {
                tool_definitions
                    .iter()
                    .enumerate()
                    .map(|(idx, tool)| ToolCall {
                        id: format!("mock-call-{}", idx + 1),
                        tool_id: tool.name.clone(),
                        arguments: serde_json::json!({}),
                    })
                    .collect()
            };

            return Ok(neuromancer_agent::llm::LlmResponse {
                text: None,
                tool_calls: calls,
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        *issued_tools = false;
        if let Some(summary) = mock_finance_summary_from_messages(&messages) {
            return Ok(neuromancer_agent::llm::LlmResponse {
                text: Some(summary),
                tool_calls: vec![],
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        }

        Ok(neuromancer_agent::llm::LlmResponse {
            text: Some("System0 turn completed.".to_string()),
            tool_calls: vec![],
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }
}

fn mock_delegate_call(messages: &[rig::completion::Message]) -> ToolCall {
    let user_text = mock_last_user_text(messages).to_ascii_lowercase();
    let is_finance_request = user_text.contains("finance")
        || user_text.contains("bill")
        || user_text.contains("account");

    if is_finance_request {
        ToolCall {
            id: "mock-call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            arguments: serde_json::json!({
                "agent_id": "finance-manager",
                "instruction": "Use manage-bills and manage-accounts to answer with due totals and available balance."
            }),
        }
    } else {
        ToolCall {
            id: "mock-call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            arguments: serde_json::json!({
                "agent_id": "planner",
                "instruction": "Summarize the user request"
            }),
        }
    }
}

fn mock_last_user_text(messages: &[rig::completion::Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|msg| match msg {
            rig::completion::Message::User { content } => {
                content.iter().find_map(|part| match part {
                    rig::message::UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn mock_finance_summary_from_messages(messages: &[rig::completion::Message]) -> Option<String> {
    let mut total_due = None;
    let mut total_balance = None;

    for msg in messages {
        let rig::completion::Message::User { content } = msg else {
            continue;
        };

        for part in content.iter() {
            let rig::message::UserContent::ToolResult(result) = part else {
                continue;
            };

            let text = result
                .content
                .iter()
                .find_map(|item| match item {
                    rig::message::ToolResultContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                continue;
            };

            match value.get("skill").and_then(|skill| skill.as_str()) {
                Some("manage-bills") => {
                    total_due = value
                        .get("script_result")
                        .and_then(|result| result.get("total_due"))
                        .and_then(|amount| amount.as_f64());
                }
                Some("manage-accounts") => {
                    total_balance = value
                        .get("script_result")
                        .and_then(|result| result.get("total_balance"))
                        .and_then(|amount| amount.as_f64())
                        .or_else(|| {
                            value
                                .get("csv")
                                .and_then(|csv| csv.as_array())
                                .and_then(|docs| docs.first())
                                .and_then(|doc| doc.get("summary"))
                                .and_then(|summary| summary.get("total_balance"))
                                .and_then(|amount| amount.as_f64())
                        });
                }
                _ => {}
            }
        }
    }

    match (total_due, total_balance) {
        (Some(due), Some(balance)) => Some(format!(
            "Finance snapshot: total_due=${:.2}, total_balance=${:.2}, remaining=${:.2}.",
            due,
            balance,
            balance - due
        )),
        _ => None,
    }
}

struct EchoLlmClient;

#[async_trait::async_trait]
impl LlmClient for EchoLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        _tool_definitions: Vec<rig::completion::ToolDefinition>,
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
