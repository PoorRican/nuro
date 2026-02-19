// Re-export message types from neuromancer-core so downstream code continues to compile.
pub use neuromancer_core::thread::{
    ChatMessage, ContentPart, MessageContent, MessageMetadata, MessageRole, TruncationStrategy,
    estimate_tokens,
};

/// Ephemeral conversation context for a single task execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationContext {
    pub messages: Vec<ChatMessage>,
    pub token_budget: u32,
    pub token_used: u32,
    pub truncation_strategy: TruncationStrategy,
}

impl ConversationContext {
    pub fn new(token_budget: u32, truncation_strategy: TruncationStrategy) -> Self {
        Self {
            messages: Vec::new(),
            token_budget,
            token_used: 0,
            truncation_strategy,
        }
    }

    pub fn add_message(&mut self, msg: ChatMessage) {
        self.token_used += msg.token_estimate;
        self.messages.push(msg);
    }

    pub fn token_count(&self) -> u32 {
        self.token_used
    }

    /// Apply truncation if budget is exceeded.
    pub fn maybe_truncate(&mut self) {
        if self.token_used <= self.token_budget {
            return;
        }

        match &self.truncation_strategy {
            TruncationStrategy::SlidingWindow { keep_last } => {
                // Always keep system messages + the last N non-system messages
                let system_msgs: Vec<ChatMessage> = self
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::System)
                    .cloned()
                    .collect();

                let non_system: Vec<ChatMessage> = self
                    .messages
                    .iter()
                    .filter(|m| m.role != MessageRole::System)
                    .cloned()
                    .collect();

                let keep_count = (*keep_last).min(non_system.len());
                let kept: Vec<ChatMessage> = non_system[non_system.len() - keep_count..].to_vec();

                self.messages = system_msgs;
                self.messages.extend(kept);
                self.recalculate_tokens();
            }
            TruncationStrategy::Summarize { threshold_pct, .. } => {
                let threshold = (self.token_budget as f32 * threshold_pct) as u32;
                if self.token_used > threshold {
                    // For now, fall back to strict truncation.
                    // Full summarization requires an LLM call and would be async.
                    self.strict_truncate();
                }
            }
            TruncationStrategy::Strict => {
                self.strict_truncate();
            }
        }
    }

    fn strict_truncate(&mut self) {
        // Drop oldest non-system messages until under budget
        while self.token_used > self.token_budget && self.messages.len() > 1 {
            // Find first non-system message
            if let Some(idx) = self
                .messages
                .iter()
                .position(|m| m.role != MessageRole::System)
            {
                let removed = self.messages.remove(idx);
                self.token_used = self.token_used.saturating_sub(removed.token_estimate);
            } else {
                break;
            }
        }
    }

    fn recalculate_tokens(&mut self) {
        self.token_used = self.messages.iter().map(|m| m.token_estimate).sum();
    }

    /// Convert conversation to rig Message format for agent.chat().
    pub fn to_rig_messages(&self) -> Vec<rig::completion::Message> {
        let mut out = Vec::new();
        for msg in &self.messages {
            match (&msg.role, &msg.content) {
                (MessageRole::User, MessageContent::Text(text)) => {
                    out.push(rig::completion::Message::user(text.clone()));
                }
                (MessageRole::Assistant, MessageContent::Text(text)) => {
                    out.push(rig::completion::Message::assistant(text.clone()));
                }
                (MessageRole::Assistant, MessageContent::ToolCalls(calls)) => {
                    // Preserve a single assistant turn even when multiple tools are requested.
                    if let Ok(content) = rig::OneOrMany::many(calls.iter().map(|call| {
                        rig::message::AssistantContent::tool_call(
                            &call.id,
                            &call.tool_id,
                            call.arguments.clone(),
                        )
                    })) {
                        out.push(rig::completion::Message::Assistant {
                            id: None,
                            content,
                        });
                    }
                }
                (MessageRole::Tool, MessageContent::ToolResult(result)) => {
                    let text = match &result.output {
                        neuromancer_core::tool::ToolOutput::Success(v) => v.to_string(),
                        neuromancer_core::tool::ToolOutput::Error(e) => {
                            format!("Error: {e}")
                        }
                    };
                    out.push(rig::completion::Message::User {
                        content: rig::OneOrMany::one(rig::message::UserContent::tool_result(
                            &result.call_id,
                            rig::OneOrMany::one(rig::message::ToolResultContent::text(text)),
                        )),
                    });
                }
                // System messages are handled via the configured system prompt, not chat history
                (MessageRole::System, _) => {}
                // Mixed content: extract text portions
                (_, MessageContent::Mixed(parts)) => {
                    for part in parts {
                        if let ContentPart::Text(text) = part {
                            out.push(rig::completion::Message::user(text.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Extract the system prompt from messages (first system message).
    pub fn system_prompt(&self) -> Option<String> {
        self.messages.iter().find_map(|m| {
            if m.role == MessageRole::System {
                if let MessageContent::Text(text) = &m.content {
                    return Some(text.clone());
                }
            }
            None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};

    #[test]
    fn add_messages_tracks_tokens() {
        let mut ctx = ConversationContext::new(1000, TruncationStrategy::Strict);
        ctx.add_message(ChatMessage::system("You are a helpful assistant."));
        ctx.add_message(ChatMessage::user("Hello!"));
        assert!(ctx.token_count() > 0);
        assert_eq!(ctx.messages.len(), 2);
    }

    #[test]
    fn strict_truncation_drops_oldest_non_system() {
        let mut ctx = ConversationContext::new(10, TruncationStrategy::Strict);
        ctx.add_message(ChatMessage::system("sys"));
        ctx.add_message(ChatMessage::user("a]".repeat(100)));
        ctx.add_message(ChatMessage::user("recent"));

        ctx.maybe_truncate();
        // System message should be preserved
        assert!(ctx.messages.iter().any(|m| m.role == MessageRole::System));
    }

    #[test]
    fn sliding_window_keeps_last_n() {
        let mut ctx =
            ConversationContext::new(10, TruncationStrategy::SlidingWindow { keep_last: 1 });
        ctx.add_message(ChatMessage::system("sys"));
        ctx.add_message(ChatMessage::user("old".repeat(100)));
        ctx.add_message(ChatMessage::user("new"));

        ctx.maybe_truncate();
        // Should have system + 1 recent
        let non_sys: Vec<_> = ctx
            .messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .collect();
        assert_eq!(non_sys.len(), 1);
    }

    #[test]
    fn to_rig_messages_converts_correctly() {
        let mut ctx = ConversationContext::new(10000, TruncationStrategy::Strict);
        ctx.add_message(ChatMessage::system("System prompt"));
        ctx.add_message(ChatMessage::user("Hello"));
        ctx.add_message(ChatMessage::assistant_text("Hi there"));

        let rig_msgs = ctx.to_rig_messages();
        // System messages are not included in rig messages (handled via system prompt)
        assert_eq!(rig_msgs.len(), 2);
    }

    #[test]
    fn to_rig_messages_keeps_multi_tool_calls_in_one_assistant_message() {
        let mut ctx = ConversationContext::new(10000, TruncationStrategy::Strict);
        ctx.add_message(ChatMessage::system("System prompt"));
        ctx.add_message(ChatMessage::assistant_tool_calls(vec![
            ToolCall {
                id: "call-1".into(),
                tool_id: "list_agents".into(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".into(),
                tool_id: "read_config".into(),
                arguments: serde_json::json!({ "section": "orchestrator" }),
            },
        ]));

        let rig_msgs = ctx.to_rig_messages();
        assert_eq!(rig_msgs.len(), 1);
        match &rig_msgs[0] {
            rig::completion::Message::Assistant { content, .. } => assert_eq!(content.len(), 2),
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn to_rig_messages_encodes_each_tool_result_as_tool_response_message() {
        let mut ctx = ConversationContext::new(10000, TruncationStrategy::Strict);
        ctx.add_message(ChatMessage::system("System prompt"));
        ctx.add_message(ChatMessage::assistant_tool_calls(vec![
            ToolCall {
                id: "call-1".into(),
                tool_id: "list_agents".into(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".into(),
                tool_id: "read_config".into(),
                arguments: serde_json::json!({}),
            },
        ]));
        ctx.add_message(ChatMessage::tool_result(ToolResult {
            call_id: "call-1".into(),
            output: ToolOutput::Success(serde_json::json!({"ok": true})),
        }));
        ctx.add_message(ChatMessage::tool_result(ToolResult {
            call_id: "call-2".into(),
            output: ToolOutput::Success(serde_json::json!({"ok": true})),
        }));

        let rig_msgs = ctx.to_rig_messages();
        assert_eq!(rig_msgs.len(), 3);

        match &rig_msgs[1] {
            rig::completion::Message::User { content } => {
                assert!(matches!(
                    content.first(),
                    rig::message::UserContent::ToolResult(_)
                ));
            }
            _ => panic!("expected first tool response as user tool_result"),
        }

        match &rig_msgs[2] {
            rig::completion::Message::User { content } => {
                assert!(matches!(
                    content.first(),
                    rig::message::UserContent::ToolResult(_)
                ));
            }
            _ => panic!("expected second tool response as user tool_result"),
        }
    }
}
