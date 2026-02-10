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
    /// `system_prompt` - the system/preamble text.
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
        // Build the current prompt from the last user message, or empty string
        let current_prompt = messages
            .last()
            .and_then(|m| match m {
                rig::completion::Message::User { content } => {
                    content.iter().find_map(|c| {
                        if let rig::message::UserContent::Text(t) = c {
                            Some(t.text.clone())
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            })
            .unwrap_or_default();

        // All messages except the last (which becomes the prompt)
        let chat_history = if messages.len() > 1 {
            messages[..messages.len() - 1].to_vec()
        } else {
            vec![]
        };

        let request = self
            .model
            .completion_request(current_prompt.clone())
            .preamble(system_prompt.to_string())
            .messages(chat_history)
            .tools(tool_definitions)
            .build();

        let response = self
            .model
            .completion(request)
            .await
            .map_err(|e| {
                NeuromancerError::Llm(neuromancer_core::error::LlmError::InvalidResponse {
                    reason: e.to_string(),
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
