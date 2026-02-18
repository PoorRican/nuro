//! Thread types: AgentThread, ThreadStore trait, and message types for conversation context.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::NeuromancerError;
use crate::tool::{ToolCall, ToolResult};

// ── Type aliases ──────────────────────────────────────────────────────────────

pub type ThreadId = String;
pub type MessageId = uuid::Uuid;
pub type ConversationId = uuid::Uuid;

// ── Message types (moved from neuromancer-agent/src/conversation.rs) ──────────

/// Role of a message in the conversation context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    ToolCalls(Vec<ToolCall>),
    ToolResult(ToolResult),
    Mixed(Vec<ContentPart>),
}

/// A single content part within a Mixed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

/// Metadata attached to a chat message.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageMetadata {
    pub source: Option<String>,
    pub parent_message_id: Option<u64>,
    pub redacted: bool,
    pub collaboration_call_id: Option<MessageId>,
    pub collaboration_result_id: Option<MessageId>,
}

/// A single message in the conversation context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    pub token_estimate: u32,
    pub metadata: MessageMetadata,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        let text = text.into();
        let estimate = estimate_tokens(&text);
        Self {
            id: MessageId::new_v4(),
            role: MessageRole::System,
            content: MessageContent::Text(text),
            timestamp: Utc::now(),
            token_estimate: estimate,
            metadata: MessageMetadata::default(),
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        let text = text.into();
        let estimate = estimate_tokens(&text);
        Self {
            id: MessageId::new_v4(),
            role: MessageRole::User,
            content: MessageContent::Text(text),
            timestamp: Utc::now(),
            token_estimate: estimate,
            metadata: MessageMetadata::default(),
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let estimate = estimate_tokens(&text);
        Self {
            id: MessageId::new_v4(),
            role: MessageRole::Assistant,
            content: MessageContent::Text(text),
            timestamp: Utc::now(),
            token_estimate: estimate,
            metadata: MessageMetadata::default(),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        let estimate = calls.len() as u32 * 50; // rough estimate per tool call
        Self {
            id: MessageId::new_v4(),
            role: MessageRole::Assistant,
            content: MessageContent::ToolCalls(calls),
            timestamp: Utc::now(),
            token_estimate: estimate,
            metadata: MessageMetadata::default(),
        }
    }

    pub fn tool_result(result: ToolResult) -> Self {
        let estimate = match &result.output {
            crate::tool::ToolOutput::Success(v) => estimate_tokens(&v.to_string()),
            crate::tool::ToolOutput::Error(e) => estimate_tokens(e),
        };
        Self {
            id: MessageId::new_v4(),
            role: MessageRole::Tool,
            content: MessageContent::ToolResult(result),
            timestamp: Utc::now(),
            token_estimate: estimate,
            metadata: MessageMetadata {
                source: Some("tool".into()),
                ..Default::default()
            },
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.metadata.source = Some(source.into());
        self
    }
}

/// Strategy for truncating conversation when token budget is exceeded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TruncationStrategy {
    /// Keep the last N messages, always preserving the system prompt.
    SlidingWindow { keep_last: usize },
    /// Summarize older messages when token_used exceeds threshold_pct of budget.
    Summarize {
        summarizer_model: String,
        threshold_pct: f32,
    },
    /// Hard truncation: drop oldest non-system messages when budget exceeded.
    Strict,
}

impl Default for TruncationStrategy {
    fn default() -> Self {
        Self::SlidingWindow { keep_last: 50 }
    }
}

/// Simple token estimate: ~4 chars per token.
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32 / 4).max(1)
}

// ── Thread types ──────────────────────────────────────────────────────────────

/// Status of a thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Active,
    Completed,
    Failed,
    Suspended,
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Suspended => write!(f, "suspended"),
        }
    }
}

/// Scope of a thread, determining its lifecycle and ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadScope {
    System0,
    Task {
        task_id: String,
    },
    UserConversation {
        conversation_id: ConversationId,
    },
    Collaboration {
        parent_thread_id: ThreadId,
        root_scope: Box<ThreadScope>,
    },
}

impl ThreadScope {
    /// Return the scope type label for DB storage.
    pub fn scope_type(&self) -> &'static str {
        match self {
            Self::System0 => "system0",
            Self::Task { .. } => "task",
            Self::UserConversation { .. } => "user_conversation",
            Self::Collaboration { .. } => "collaboration",
        }
    }

    /// Return the primary scope reference (e.g. task_id or parent_thread_id).
    pub fn scope_ref(&self) -> Option<String> {
        match self {
            Self::System0 => None,
            Self::Task { task_id } => Some(task_id.clone()),
            Self::UserConversation { conversation_id } => Some(conversation_id.to_string()),
            Self::Collaboration {
                parent_thread_id, ..
            } => Some(parent_thread_id.clone()),
        }
    }
}

/// Compaction policy for a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionPolicy {
    SummarizeToMemory {
        target_partition: String,
        summarizer_model: String,
        threshold_pct: f32,
    },
    InPlace {
        strategy: TruncationStrategy,
    },
    None,
}

/// A persistent thread record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentThread {
    pub id: ThreadId,
    pub agent_id: String,
    pub scope: ThreadScope,
    pub compaction_policy: CompactionPolicy,
    pub context_window_budget: u32,
    pub status: ThreadStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A cross-reference from one thread's message to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossReference {
    pub thread_id: ThreadId,
    pub message_id: MessageId,
    pub role: String,
    pub content_preview: String,
}

// ── ThreadStore trait ─────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait ThreadStore: Send + Sync {
    async fn create_thread(&self, thread: &AgentThread) -> Result<(), NeuromancerError>;

    async fn get_thread(&self, thread_id: &ThreadId) -> Result<Option<AgentThread>, NeuromancerError>;

    async fn update_status(
        &self,
        thread_id: &ThreadId,
        status: ThreadStatus,
    ) -> Result<(), NeuromancerError>;

    async fn list_threads_by_scope_type(
        &self,
        scope_type: &str,
    ) -> Result<Vec<AgentThread>, NeuromancerError>;

    async fn list_threads_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<AgentThread>, NeuromancerError>;

    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: &[ChatMessage],
    ) -> Result<(), NeuromancerError>;

    async fn load_messages(
        &self,
        thread_id: &ThreadId,
        include_compacted: bool,
    ) -> Result<Vec<ChatMessage>, NeuromancerError>;

    async fn find_collaboration_thread(
        &self,
        agent_id: &str,
        parent_thread_id: &ThreadId,
    ) -> Result<Option<AgentThread>, NeuromancerError>;

    async fn resolve_cross_reference(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<CrossReference>, NeuromancerError>;

    async fn mark_compacted(
        &self,
        thread_id: &ThreadId,
        up_to_message_id: &MessageId,
    ) -> Result<(), NeuromancerError>;

    /// Return the total estimated token count of un-compacted messages in a thread.
    async fn total_uncompacted_tokens(
        &self,
        thread_id: &ThreadId,
    ) -> Result<u32, NeuromancerError>;
}
