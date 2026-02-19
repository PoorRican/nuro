use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use neuromancer_core::error::NeuromancerError;
use neuromancer_core::tool::ToolCall;

/// An LLM completion response that our agent runtime works with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Text content from the assistant (if any).
    pub text: Option<String>,
    /// Tool calls requested by the assistant (if any).
    pub tool_calls: Vec<ToolCall>,
    /// Token usage for this call.
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl LlmResponse {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    pub fn is_text_only(&self) -> bool {
        self.tool_calls.is_empty() && self.text.is_some()
    }
}

/// Abstraction over LLM completion that the agent runtime uses.
/// This decouples the agent state machine from any specific LLM provider.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Perform a completion call.
    ///
    /// `system_prompt` - the system instruction text.
    /// `messages` - chat history in rig Message format.
    /// `tool_definitions` - available tools for this call.
    async fn complete(
        &self,
        system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<LlmResponse, NeuromancerError>;
}

/// An LlmClient implementation that wraps a rig CompletionModel.
pub struct RigLlmClient<M: rig::completion::CompletionModel> {
    model: M,
}

impl<M: rig::completion::CompletionModel> RigLlmClient<M> {
    pub fn new(model: M) -> Self {
        Self { model }
    }
}

#[async_trait]
impl<M> LlmClient for RigLlmClient<M>
where
    M: rig::completion::CompletionModel + Send + Sync + 'static,
    M::Response: Send + Sync,
{
    async fn complete(
        &self,
        system_prompt: &str,
        messages: Vec<rig::completion::Message>,
        tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<LlmResponse, NeuromancerError> {
        let (current_prompt, chat_history) = split_prompt_and_history(messages);

        let request = self
            .model
            .completion_request(current_prompt.clone())
            .messages(chat_history)
            .tools(tool_definitions)
            .build();

        let history_vec: Vec<_> = request.chat_history.iter().collect();
        let request_debug = render_request_debug(
            system_prompt,
            &current_prompt,
            &history_vec,
            &request.tools,
        );
        let response = self.model.completion(request).await.map_err(|e| {
            NeuromancerError::Llm(neuromancer_core::error::LlmError::InvalidResponse {
                reason: format!(
                    "{}\nrequest_debug: {}",
                    e,
                    truncate_debug(&request_debug, 8_000)
                ),
            })
        })?;

        // Parse the response into our LlmResponse
        let mut text = None;
        let mut tool_calls = Vec::new();

        for content in response.choice.iter() {
            match content {
                rig::message::AssistantContent::Text(t) => {
                    text = Some(t.text.clone());
                }
                rig::message::AssistantContent::ToolCall(tc) => {
                    tool_calls.push(ToolCall {
                        id: tc.id.clone(),
                        tool_id: tc.function.name.clone(),
                        arguments: tc.function.arguments.clone(),
                    });
                }
                _ => {} // Reasoning, Image, etc. — ignored for now
            }
        }

        Ok(LlmResponse {
            text,
            tool_calls,
            // Token counts not exposed by rig's CompletionResponse, estimate from content
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }
}

fn split_prompt_and_history(
    messages: Vec<rig::completion::Message>,
) -> (String, Vec<rig::completion::Message>) {
    let Some(last) = messages.last() else {
        return (String::new(), vec![]);
    };

    if let Some(text) = extract_user_text(last) {
        let history = if messages.len() > 1 {
            messages[..messages.len() - 1].to_vec()
        } else {
            vec![]
        };
        return (text, history);
    }

    (String::new(), messages)
}

fn extract_user_text(message: &rig::completion::Message) -> Option<String> {
    match message {
        rig::completion::Message::User { content } => content.iter().find_map(|c| {
            if let rig::message::UserContent::Text(t) = c {
                Some(t.text.clone())
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn render_request_debug(
    system_prompt: &str,
    current_prompt: &str,
    chat_history: &[&rig::completion::Message],
    tools: &[rig::completion::ToolDefinition],
) -> String {
    serde_json::json!({
        "system_prompt_preview": truncate_debug(system_prompt, 500),
        "current_prompt_preview": truncate_debug(current_prompt, 500),
        "chat_history": chat_history,
        "tools": tools,
    })
    .to_string()
}

fn truncate_debug(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_chars).collect();
    format!("{}...(+{} chars)", truncated, char_count - max_chars)
}

/// A mock LLM client for testing.
pub struct MockLlmClient {
    responses: std::sync::Mutex<Vec<LlmResponse>>,
}

impl MockLlmClient {
    pub fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(
        &self,
        _system_prompt: &str,
        _messages: Vec<rig::completion::Message>,
        _tool_definitions: Vec<rig::completion::ToolDefinition>,
    ) -> Result<LlmResponse, NeuromancerError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(LlmResponse {
                text: Some("No more mock responses".into()),
                tool_calls: vec![],
                prompt_tokens: 0,
                completion_tokens: 0,
            })
        } else {
            Ok(responses.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_prompt_uses_last_user_text_as_prompt() {
        let messages = vec![
            rig::completion::Message::assistant("hello"),
            rig::completion::Message::user("what now"),
        ];

        let (prompt, history) = split_prompt_and_history(messages);
        assert_eq!(prompt, "what now");
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn split_prompt_keeps_tool_result_in_history() {
        let messages = vec![
            rig::completion::Message::user("question"),
            rig::completion::Message::Assistant {
                id: None,
                content: rig::OneOrMany::one(rig::message::AssistantContent::tool_call(
                    "call-1",
                    "list_agents",
                    serde_json::json!({}),
                )),
            },
            rig::completion::Message::User {
                content: rig::OneOrMany::one(rig::message::UserContent::tool_result(
                    "call-1",
                    rig::OneOrMany::one(rig::message::ToolResultContent::text("[]")),
                )),
            },
        ];

        let (prompt, history) = split_prompt_and_history(messages);
        assert_eq!(prompt, "");
        assert_eq!(history.len(), 3);
    }
}
