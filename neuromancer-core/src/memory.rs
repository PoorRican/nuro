use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::NeuromancerError;
use crate::tool::AgentContext;

pub type MemoryId = uuid::Uuid;

/// A single memory record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: MemoryId,
    pub partition: String,
    pub kind: MemoryKind,
    pub content: String,
    pub tags: Vec<String>,
    pub source: MemorySource,
    pub sensitivity: Sensitivity,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: serde_json::Value,
}

impl MemoryItem {
    pub fn new(partition: String, kind: MemoryKind, content: String) -> Self {
        Self {
            id: MemoryId::new_v4(),
            partition,
            kind,
            content,
            tags: Vec::new(),
            source: MemorySource::System,
            sensitivity: Sensitivity::Public,
            created_at: Utc::now(),
            expires_at: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_source(mut self, source: MemorySource) -> Self {
        self.source = source;
        self
    }

    pub fn with_ttl(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Message,
    Summary,
    Fact,
    Artifact,
    Link,
    ToolResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    Discord,
    Cron,
    A2a,
    Tool,
    Agent,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Private,
    SecretRefOnly,
}

/// Query parameters for memory retrieval.
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    pub partition: Option<String>,
    pub kind: Option<MemoryKind>,
    pub tags: Vec<String>,
    pub text_search: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

/// Paginated memory query result.
#[derive(Debug, Clone)]
pub struct MemoryPage {
    pub items: Vec<MemoryItem>,
    pub total: u64,
    pub has_more: bool,
}

/// Persistent, partitioned memory store.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn put(&self, ctx: &AgentContext, item: MemoryItem)
    -> Result<MemoryId, NeuromancerError>;
    async fn query(
        &self,
        ctx: &AgentContext,
        q: MemoryQuery,
    ) -> Result<MemoryPage, NeuromancerError>;
    async fn get(
        &self,
        ctx: &AgentContext,
        id: MemoryId,
    ) -> Result<Option<MemoryItem>, NeuromancerError>;
    async fn delete(&self, ctx: &AgentContext, id: MemoryId) -> Result<(), NeuromancerError>;
    /// Garbage-collect expired items. Returns the number of items removed.
    async fn gc(&self) -> Result<u64, NeuromancerError>;
}
