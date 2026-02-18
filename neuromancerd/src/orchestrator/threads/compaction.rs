//! SummarizeToMemory compaction: summarize old messages into memory items and mark them compacted.

use neuromancer_core::error::NeuromancerError;
use neuromancer_core::memory::{MemoryItem, MemoryKind, MemorySource};
use neuromancer_core::memory::MemoryStore;
use neuromancer_core::thread::{
    AgentThread, ChatMessage, CompactionPolicy, MessageContent, MessageRole, ThreadStore,
};
use neuromancer_core::tool::AgentContext;

/// Number of recent messages to preserve (never compact the tail).
const RECENT_WINDOW: usize = 10;

/// Minimum messages required before considering compaction.
const MIN_MESSAGES_FOR_COMPACTION: usize = RECENT_WINDOW + 5;

/// Check if a thread needs compaction and perform it if so.
///
/// For `SummarizeToMemory` policy:
///   1. Check total uncompacted tokens vs threshold
///   2. Load uncompacted messages, select oldest outside recent window
///   3. Summarize them into a memory item
///   4. Mark them as compacted
pub async fn maybe_compact_thread(
    thread_store: &dyn ThreadStore,
    memory_store: &dyn MemoryStore,
    agent_ctx: &AgentContext,
    thread: &AgentThread,
) -> Result<bool, NeuromancerError> {
    let (target_partition, threshold_pct) = match &thread.compaction_policy {
        CompactionPolicy::SummarizeToMemory {
            target_partition,
            threshold_pct,
            ..
        } => (target_partition.clone(), *threshold_pct),
        _ => return Ok(false),
    };

    let total_tokens = thread_store.total_uncompacted_tokens(&thread.id).await?;
    let threshold = (thread.context_window_budget as f32 * threshold_pct) as u32;

    if total_tokens <= threshold {
        return Ok(false);
    }

    tracing::info!(
        thread_id = %thread.id,
        total_tokens,
        threshold,
        budget = thread.context_window_budget,
        "compaction triggered"
    );

    // Load all uncompacted messages
    let messages = thread_store.load_messages(&thread.id, false).await?;
    if messages.len() < MIN_MESSAGES_FOR_COMPACTION {
        return Ok(false);
    }

    // Split: compact the older messages, keep the recent window
    let compact_end = messages.len().saturating_sub(RECENT_WINDOW);
    let to_compact = &messages[..compact_end];

    if to_compact.is_empty() {
        return Ok(false);
    }

    // Build a text summary of the compacted messages
    let summary = build_summary(to_compact);

    // Store summary in memory
    let memory_item = MemoryItem::new(target_partition, MemoryKind::Summary, summary)
        .with_source(MemorySource::System)
        .with_tags(vec![
            format!("thread:{}", thread.id),
            "compaction_summary".to_string(),
        ]);
    memory_store.put(agent_ctx, memory_item).await?;

    // Mark messages as compacted (up to the last message in the compact range)
    let last_compacted = &to_compact[to_compact.len() - 1];
    thread_store
        .mark_compacted(&thread.id, &last_compacted.id)
        .await?;

    tracing::info!(
        thread_id = %thread.id,
        compacted_count = to_compact.len(),
        remaining = messages.len() - compact_end,
        "compaction completed"
    );

    Ok(true)
}

/// Build a textual summary from a slice of messages.
///
/// This produces a structured digest. A future enhancement would use an LLM
/// for higher-quality summarization.
fn build_summary(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Conversation summary ({} messages):\n",
        messages.len()
    ));

    for msg in messages {
        let role = match &msg.role {
            MessageRole::System => continue, // skip system messages in summary
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
        };

        let text = match &msg.content {
            MessageContent::Text(t) => truncate_for_summary(t, 200),
            MessageContent::ToolCalls(calls) => {
                let names: Vec<&str> = calls.iter().map(|c| c.tool_id.as_str()).collect();
                format!("[called tools: {}]", names.join(", "))
            }
            MessageContent::ToolResult(r) => {
                format!("[tool result for {}]", r.call_id)
            }
            MessageContent::Mixed(parts) => {
                let texts: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match p {
                        neuromancer_core::thread::ContentPart::Text(t) => {
                            Some(truncate_for_summary(t, 100))
                        }
                        _ => None,
                    })
                    .collect();
                texts.join("; ")
            }
        };

        parts.push(format!("- {role}: {text}"));
    }

    parts.join("\n")
}

fn truncate_for_summary(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        // Find a safe char boundary
        let mut end = max_len;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &text[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::thread::MessageRole;

    #[test]
    fn build_summary_produces_readable_output() {
        let msgs = vec![
            ChatMessage::user("What is the capital of France?"),
            ChatMessage::assistant_text("The capital of France is Paris."),
            ChatMessage::user("And Germany?"),
            ChatMessage::assistant_text("The capital of Germany is Berlin."),
        ];

        let summary = build_summary(&msgs);
        assert!(summary.contains("User: What is the capital of France?"));
        assert!(summary.contains("Assistant: The capital of France is Paris."));
        assert!(summary.contains("4 messages"));
    }

    #[test]
    fn build_summary_skips_system_messages() {
        let msgs = vec![
            ChatMessage::system("You are a helpful assistant"),
            ChatMessage::user("Hello"),
        ];

        let summary = build_summary(&msgs);
        assert!(!summary.contains("System"));
        assert!(summary.contains("User: Hello"));
    }

    #[test]
    fn truncate_handles_multibyte() {
        let text = "Hello \u{2019}world\u{2014}test";
        let result = truncate_for_summary(text, 8);
        assert!(result.ends_with("..."));
        assert!(result.len() < text.len() + 3);
    }
}
