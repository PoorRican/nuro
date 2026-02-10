use async_trait::async_trait;
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{info, instrument};

use neuromancer_core::error::{InfraError, NeuromancerError, PolicyError};
use neuromancer_core::memory::{
    MemoryId, MemoryItem, MemoryKind, MemoryPage, MemoryQuery, MemorySource, MemoryStore,
    Sensitivity,
};
use neuromancer_core::tool::AgentContext;

/// SQLite-backed partitioned memory store.
pub struct SqliteMemoryStore {
    pool: SqlitePool,
}

impl SqliteMemoryStore {
    pub async fn new(pool: SqlitePool) -> Result<Self, NeuromancerError> {
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS memory_items (
                id TEXT PRIMARY KEY,
                partition TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                source TEXT NOT NULL,
                sensitivity TEXT NOT NULL DEFAULT 'public',
                created_at TEXT NOT NULL,
                expires_at TEXT,
                metadata TEXT NOT NULL DEFAULT 'null'
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        // Indexes for common query patterns
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_partition ON memory_items(partition)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_kind ON memory_items(kind)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_expires ON memory_items(expires_at)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        Ok(())
    }

    /// Check that the agent is allowed to access the given partition.
    fn check_partition_access(
        ctx: &AgentContext,
        partition: &str,
    ) -> Result<(), NeuromancerError> {
        if ctx.allowed_memory_partitions.contains(&partition.to_string()) {
            return Ok(());
        }
        Err(NeuromancerError::Policy(
            PolicyError::PartitionAccessDenied {
                agent_id: ctx.agent_id.clone(),
                partition: partition.to_string(),
            },
        ))
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    #[instrument(skip(self, ctx, item), fields(agent_id = %ctx.agent_id, partition = %item.partition))]
    async fn put(
        &self,
        ctx: &AgentContext,
        item: MemoryItem,
    ) -> Result<MemoryId, NeuromancerError> {
        Self::check_partition_access(ctx, &item.partition)?;

        let id = item.id.to_string();
        let kind = serde_json::to_string(&item.kind)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        // Strip surrounding quotes from the serialized enum variant
        let kind = kind.trim_matches('"');
        let tags = serde_json::to_string(&item.tags)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let source = serde_json::to_string(&item.source)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let source = source.trim_matches('"');
        let sensitivity = serde_json::to_string(&item.sensitivity)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let sensitivity = sensitivity.trim_matches('"');
        let created_at = item.created_at.to_rfc3339();
        let expires_at = item.expires_at.map(|t| t.to_rfc3339());
        let metadata = serde_json::to_string(&item.metadata)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        sqlx::query(
            r#"
            INSERT INTO memory_items (id, partition, kind, content, tags, source, sensitivity, created_at, expires_at, metadata)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(&item.partition)
        .bind(kind)
        .bind(&item.content)
        .bind(&tags)
        .bind(source)
        .bind(sensitivity)
        .bind(&created_at)
        .bind(&expires_at)
        .bind(&metadata)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        info!(memory_id = %item.id, partition = %item.partition, "memory item stored");
        Ok(item.id)
    }

    #[instrument(skip(self, ctx, q), fields(agent_id = %ctx.agent_id))]
    async fn query(
        &self,
        ctx: &AgentContext,
        q: MemoryQuery,
    ) -> Result<MemoryPage, NeuromancerError> {
        // If a partition filter is specified, check access
        if let Some(ref partition) = q.partition {
            Self::check_partition_access(ctx, partition)?;
        }

        // Build dynamic query
        let mut conditions = Vec::new();
        let mut bind_values: Vec<String> = Vec::new();

        // Always filter to only allowed partitions
        if let Some(ref partition) = q.partition {
            conditions.push("partition = ?".to_string());
            bind_values.push(partition.clone());
        } else {
            // Filter to agent's allowed partitions
            let placeholders: Vec<&str> = ctx
                .allowed_memory_partitions
                .iter()
                .map(|_| "?")
                .collect();
            if placeholders.is_empty() {
                return Ok(MemoryPage {
                    items: vec![],
                    total: 0,
                    has_more: false,
                });
            }
            conditions.push(format!("partition IN ({})", placeholders.join(", ")));
            bind_values.extend(ctx.allowed_memory_partitions.iter().cloned());
        }

        if let Some(ref kind) = q.kind {
            let kind_str = serde_json::to_string(kind)
                .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
            let kind_str = kind_str.trim_matches('"').to_string();
            conditions.push("kind = ?".to_string());
            bind_values.push(kind_str);
        }

        if let Some(ref text) = q.text_search {
            conditions.push("content LIKE ?".to_string());
            bind_values.push(format!("%{text}%"));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // Count total
        let count_sql = format!("SELECT COUNT(*) as cnt FROM memory_items {where_clause}");
        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        for val in &bind_values {
            count_query = count_query.bind(val);
        }
        let total = count_query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?
            as u64;

        // Fetch page
        let limit = if q.limit == 0 { 100 } else { q.limit };
        let data_sql = format!(
            "SELECT id, partition, kind, content, tags, source, sensitivity, created_at, expires_at, metadata FROM memory_items {where_clause} ORDER BY created_at DESC LIMIT ? OFFSET ?"
        );
        let mut data_query = sqlx::query_as::<_, MemoryRow>(&data_sql);
        for val in &bind_values {
            data_query = data_query.bind(val);
        }
        data_query = data_query.bind(limit as i64).bind(q.offset as i64);

        let rows = data_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        // Tag filtering in application layer (SQLite JSON support varies)
        let items: Vec<MemoryItem> = rows
            .into_iter()
            .filter_map(|row| row.to_memory_item().ok())
            .filter(|item| {
                if q.tags.is_empty() {
                    return true;
                }
                q.tags.iter().any(|t| item.tags.contains(t))
            })
            .collect();

        let has_more = (q.offset + limit) < total as usize;

        Ok(MemoryPage {
            items,
            total,
            has_more,
        })
    }

    #[instrument(skip(self, ctx), fields(agent_id = %ctx.agent_id, memory_id = %id))]
    async fn get(
        &self,
        ctx: &AgentContext,
        id: MemoryId,
    ) -> Result<Option<MemoryItem>, NeuromancerError> {
        let row = sqlx::query_as::<_, MemoryRow>(
            "SELECT id, partition, kind, content, tags, source, sensitivity, created_at, expires_at, metadata FROM memory_items WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let Some(row) = row else {
            return Ok(None);
        };

        // Check partition access
        Self::check_partition_access(ctx, &row.partition)?;

        let item = row
            .to_memory_item()
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e)))?;

        Ok(Some(item))
    }

    #[instrument(skip(self, ctx), fields(agent_id = %ctx.agent_id, memory_id = %id))]
    async fn delete(
        &self,
        ctx: &AgentContext,
        id: MemoryId,
    ) -> Result<(), NeuromancerError> {
        // First check access by fetching the partition
        let partition: Option<String> = sqlx::query_scalar(
            "SELECT partition FROM memory_items WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        if let Some(ref partition) = partition {
            Self::check_partition_access(ctx, partition)?;
        }

        sqlx::query("DELETE FROM memory_items WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        info!(memory_id = %id, "memory item deleted");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn gc(&self) -> Result<u64, NeuromancerError> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "DELETE FROM memory_items WHERE expires_at IS NOT NULL AND expires_at < ?",
        )
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let removed = result.rows_affected();
        if removed > 0 {
            info!(removed = removed, "garbage collected expired memory items");
        }
        Ok(removed)
    }
}

/// Internal row representation for SQLite queries.
#[derive(Debug, sqlx::FromRow)]
struct MemoryRow {
    id: String,
    partition: String,
    kind: String,
    content: String,
    tags: String,
    source: String,
    sensitivity: String,
    created_at: String,
    expires_at: Option<String>,
    metadata: String,
}

impl MemoryRow {
    fn to_memory_item(&self) -> Result<MemoryItem, String> {
        let id: MemoryId = self
            .id
            .parse()
            .map_err(|e| format!("invalid uuid: {e}"))?;

        let kind: MemoryKind = serde_json::from_str(&format!("\"{}\"", self.kind))
            .map_err(|e| format!("invalid kind '{}': {e}", self.kind))?;

        let tags: Vec<String> =
            serde_json::from_str(&self.tags).map_err(|e| format!("invalid tags: {e}"))?;

        let source: MemorySource = serde_json::from_str(&format!("\"{}\"", self.source))
            .map_err(|e| format!("invalid source '{}': {e}", self.source))?;

        let sensitivity: Sensitivity =
            serde_json::from_str(&format!("\"{}\"", self.sensitivity))
                .map_err(|e| format!("invalid sensitivity '{}': {e}", self.sensitivity))?;

        let created_at = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| format!("invalid created_at: {e}"))?;

        let expires_at = self
            .expires_at
            .as_ref()
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| format!("invalid expires_at: {e}"))
            })
            .transpose()?;

        let metadata: serde_json::Value =
            serde_json::from_str(&self.metadata).unwrap_or(serde_json::Value::Null);

        Ok(MemoryItem {
            id,
            partition: self.partition.clone(),
            kind,
            content: self.content.clone(),
            tags,
            source,
            sensitivity,
            created_at,
            expires_at,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite")
    }

    fn test_ctx(agent_id: &str, partitions: Vec<&str>) -> AgentContext {
        AgentContext {
            agent_id: agent_id.to_string(),
            task_id: uuid::Uuid::new_v4(),
            allowed_tools: vec![],
            allowed_mcp_servers: vec![],
            allowed_secrets: vec![],
            allowed_memory_partitions: partitions.into_iter().map(String::from).collect(),
        }
    }

    #[tokio::test]
    async fn put_and_get() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);
        let item = MemoryItem::new(
            "agent:browser".into(),
            MemoryKind::Fact,
            "The user prefers dark mode.".into(),
        );
        let id = item.id;

        store.put(&ctx, item).await.unwrap();

        let retrieved = store.get(&ctx, id).await.unwrap().unwrap();
        assert_eq!(retrieved.content, "The user prefers dark mode.");
        assert_eq!(retrieved.kind, MemoryKind::Fact);
        assert_eq!(retrieved.partition, "agent:browser");
    }

    #[tokio::test]
    async fn partition_access_denied() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);
        let item = MemoryItem::new(
            "agent:planner".into(), // not in browser's allowed partitions
            MemoryKind::Fact,
            "should fail".into(),
        );

        let err = store.put(&ctx, item).await.unwrap_err();
        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::PartitionAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn query_by_partition() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser", "workspace:default"]);

        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "fact 1".into()),
            )
            .await
            .unwrap();
        store
            .put(
                &ctx,
                MemoryItem::new("workspace:default".into(), MemoryKind::Summary, "sum 1".into()),
            )
            .await
            .unwrap();

        let page = store
            .query(
                &ctx,
                MemoryQuery {
                    partition: Some("agent:browser".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content, "fact 1");
    }

    #[tokio::test]
    async fn query_by_kind() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);

        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "a fact".into()),
            )
            .await
            .unwrap();
        store
            .put(
                &ctx,
                MemoryItem::new(
                    "agent:browser".into(),
                    MemoryKind::Summary,
                    "a summary".into(),
                ),
            )
            .await
            .unwrap();

        let page = store
            .query(
                &ctx,
                MemoryQuery {
                    kind: Some(MemoryKind::Fact),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content, "a fact");
    }

    #[tokio::test]
    async fn query_text_search() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);

        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "dark mode is preferred".into()),
            )
            .await
            .unwrap();
        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "light theme selected".into()),
            )
            .await
            .unwrap();

        let page = store
            .query(
                &ctx,
                MemoryQuery {
                    text_search: Some("dark".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert!(page.items[0].content.contains("dark"));
    }

    #[tokio::test]
    async fn query_by_tags() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);

        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "tagged item".into())
                    .with_tags(vec!["preference".into(), "ui".into()]),
            )
            .await
            .unwrap();
        store
            .put(
                &ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "untagged".into()),
            )
            .await
            .unwrap();

        let page = store
            .query(
                &ctx,
                MemoryQuery {
                    tags: vec!["preference".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content, "tagged item");
    }

    #[tokio::test]
    async fn delete_item() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);
        let item = MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "to delete".into());
        let id = item.id;

        store.put(&ctx, item).await.unwrap();
        store.delete(&ctx, id).await.unwrap();

        let result = store.get(&ctx, id).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn gc_removes_expired_items() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);

        // Expired item
        let expired = MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "expired".into())
            .with_ttl(Utc::now() - Duration::hours(1));
        let expired_id = expired.id;
        store.put(&ctx, expired).await.unwrap();

        // Non-expired item
        let fresh = MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "fresh".into())
            .with_ttl(Utc::now() + Duration::hours(1));
        let fresh_id = fresh.id;
        store.put(&ctx, fresh).await.unwrap();

        let removed = store.gc().await.unwrap();
        assert_eq!(removed, 1);

        assert!(store.get(&ctx, expired_id).await.unwrap().is_none());
        assert!(store.get(&ctx, fresh_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn pagination() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let ctx = test_ctx("browser", vec!["agent:browser"]);

        for i in 0..5 {
            store
                .put(
                    &ctx,
                    MemoryItem::new(
                        "agent:browser".into(),
                        MemoryKind::Fact,
                        format!("item {i}"),
                    ),
                )
                .await
                .unwrap();
        }

        let page = store
            .query(
                &ctx,
                MemoryQuery {
                    limit: 2,
                    offset: 0,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 5);
        assert!(page.has_more);

        let page2 = store
            .query(
                &ctx,
                MemoryQuery {
                    limit: 2,
                    offset: 4,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page2.items.len(), 1);
        assert!(!page2.has_more);
    }

    #[tokio::test]
    async fn query_without_partition_returns_all_allowed() {
        let pool = test_pool().await;
        let store = SqliteMemoryStore::new(pool).await.unwrap();

        let admin_ctx = test_ctx("admin", vec!["agent:browser", "agent:planner"]);

        store
            .put(
                &admin_ctx,
                MemoryItem::new("agent:browser".into(), MemoryKind::Fact, "browser fact".into()),
            )
            .await
            .unwrap();
        store
            .put(
                &admin_ctx,
                MemoryItem::new("agent:planner".into(), MemoryKind::Fact, "planner fact".into()),
            )
            .await
            .unwrap();

        // Agent with access to only browser
        let browser_ctx = test_ctx("browser", vec!["agent:browser"]);
        let page = store
            .query(&browser_ctx, MemoryQuery::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content, "browser fact");

        // Agent with access to both
        let page = store
            .query(&admin_ctx, MemoryQuery::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
    }
}
