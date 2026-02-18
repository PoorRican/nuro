use std::sync::Arc;

use async_trait::async_trait;

use neuromancer_core::error::NeuromancerError;
use neuromancer_core::memory::{MemoryId, MemoryItem, MemoryPage, MemoryQuery, MemoryStore};
use neuromancer_core::secrets::TextRedactor;
use neuromancer_core::tool::AgentContext;

/// Wrapper around a `MemoryStore` that redacts secret values from memory item
/// content before writing. Prevents accidental secret persistence in the
/// memory layer.
pub struct RedactingMemoryStore {
    inner: Arc<dyn MemoryStore>,
    redactor: Arc<dyn TextRedactor>,
}

impl RedactingMemoryStore {
    pub fn new(inner: Arc<dyn MemoryStore>, redactor: Arc<dyn TextRedactor>) -> Self {
        Self { inner, redactor }
    }
}

#[async_trait]
impl MemoryStore for RedactingMemoryStore {
    async fn put(
        &self,
        ctx: &AgentContext,
        mut item: MemoryItem,
    ) -> Result<MemoryId, NeuromancerError> {
        item.content = self.redactor.redact(&item.content);
        self.inner.put(ctx, item).await
    }

    async fn query(
        &self,
        ctx: &AgentContext,
        q: MemoryQuery,
    ) -> Result<MemoryPage, NeuromancerError> {
        self.inner.query(ctx, q).await
    }

    async fn get(
        &self,
        ctx: &AgentContext,
        id: MemoryId,
    ) -> Result<Option<MemoryItem>, NeuromancerError> {
        self.inner.get(ctx, id).await
    }

    async fn delete(&self, ctx: &AgentContext, id: MemoryId) -> Result<(), NeuromancerError> {
        self.inner.delete(ctx, id).await
    }

    async fn gc(&self) -> Result<u64, NeuromancerError> {
        self.inner.gc().await
    }
}
