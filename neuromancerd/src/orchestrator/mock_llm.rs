use std::sync::Mutex;

use neuromancer_agent::llm::LlmClient;
use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::ToolCall;

#[derive(Default)]
pub(crate) struct TwoStepMockLlmClient {
    issued_tools: Mutex<bool>,
}

#[async_trait::async_trait]
impl LlmClient for TwoStepMockLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
        _output_schema: Option<schemars::Schema>,
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
                "instruction": "{{USER_QUERY}}\nUse manage-bills and manage-accounts to answer with due totals and available balance."
            }),
        }
    } else {
        ToolCall {
            id: "mock-call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            arguments: serde_json::json!({
                "agent_id": "planner",
                "instruction": "{{USER_QUERY}}"
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
