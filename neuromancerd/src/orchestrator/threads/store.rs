use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use neuromancer_core::error::{InfraError, NeuromancerError};
use neuromancer_core::thread::{
    AgentThread, ChatMessage, CompactionPolicy, ConversationId, CrossReference, MessageContent,
    MessageId, MessageMetadata, MessageRole, ThreadId, ThreadScope, ThreadStatus,
    UserConversation,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

#[derive(Clone)]
pub(crate) struct SqliteThreadStore {
    pool: Arc<SqlitePool>,
}

impl SqliteThreadStore {
    pub(crate) async fn open(path: &Path) -> Result<Self, NeuromancerError> {
        let parent = path.parent().ok_or_else(|| {
            NeuromancerError::Infra(InfraError::Config(format!(
                "invalid thread db path '{}'",
                path.display()
            )))
        })?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| NeuromancerError::Infra(InfraError::Io(err)))?;

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(|err| {
                NeuromancerError::Infra(InfraError::Config(format!(
                    "invalid sqlite options: {err}"
                )))
            })?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        let store = Self {
            pool: Arc::new(pool),
        };
        store.migrate().await?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) async fn in_memory() -> Result<Self, NeuromancerError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        let store = Self {
            pool: Arc::new(pool),
        };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                scope_type TEXT NOT NULL,
                scope_ref TEXT,
                root_scope_type TEXT,
                root_scope_ref TEXT,
                compaction_policy_json TEXT NOT NULL,
                context_window_budget INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS thread_messages (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content_json TEXT NOT NULL,
                token_estimate INTEGER NOT NULL,
                metadata_json TEXT NOT NULL,
                compacted INTEGER NOT NULL DEFAULT 0,
                timestamp TEXT NOT NULL,
                FOREIGN KEY (thread_id) REFERENCES threads(id)
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_thread_messages_thread_id
            ON thread_messages(thread_id, timestamp)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_threads_agent_id
            ON threads(agent_id)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_threads_scope
            ON threads(scope_type, scope_ref)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS user_conversations (
                conversation_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY (thread_id) REFERENCES threads(id)
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_user_conversations_agent
            ON user_conversations(agent_id, status)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        Ok(())
    }
}

// ── ThreadScope serialization helpers ────────────────────────────────────────

fn scope_to_columns(
    scope: &ThreadScope,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let scope_type = scope.scope_type().to_string();
    let scope_ref = scope.scope_ref();
    match scope {
        ThreadScope::Collaboration { root_scope, .. } => {
            let root_type = Some(root_scope.scope_type().to_string());
            let root_ref = root_scope.scope_ref();
            (scope_type, scope_ref, root_type, root_ref)
        }
        _ => (scope_type, scope_ref, None, None),
    }
}

fn columns_to_scope(
    scope_type: &str,
    scope_ref: Option<&str>,
    root_scope_type: Option<&str>,
    root_scope_ref: Option<&str>,
) -> ThreadScope {
    match scope_type {
        "system0" => ThreadScope::System0,
        "task" => ThreadScope::Task {
            task_id: scope_ref.unwrap_or_default().to_string(),
        },
        "user_conversation" => ThreadScope::UserConversation {
            conversation_id: scope_ref
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(uuid::Uuid::new_v4),
        },
        "collaboration" => {
            let root_scope = match root_scope_type {
                Some(rt) => columns_to_scope(rt, root_scope_ref, None, None),
                None => ThreadScope::System0,
            };
            ThreadScope::Collaboration {
                parent_thread_id: scope_ref.unwrap_or_default().to_string(),
                root_scope: Box::new(root_scope),
            }
        }
        _ => ThreadScope::System0,
    }
}

// ── Row parsing helpers ──────────────────────────────────────────────────────

fn parse_thread_row(row: &sqlx::sqlite::SqliteRow) -> Result<AgentThread, NeuromancerError> {
    let id: String = row
        .try_get("id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let agent_id: String = row
        .try_get("agent_id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let scope_type: String = row
        .try_get("scope_type")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let scope_ref: Option<String> = row
        .try_get("scope_ref")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let root_scope_type: Option<String> = row
        .try_get("root_scope_type")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let root_scope_ref: Option<String> = row
        .try_get("root_scope_ref")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let compaction_json: String = row
        .try_get("compaction_policy_json")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let context_window_budget: i64 = row
        .try_get("context_window_budget")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let status_str: String = row
        .try_get("status")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let created_at_str: String = row
        .try_get("created_at")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let updated_at_str: String = row
        .try_get("updated_at")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    let scope = columns_to_scope(
        &scope_type,
        scope_ref.as_deref(),
        root_scope_type.as_deref(),
        root_scope_ref.as_deref(),
    );
    let compaction_policy: CompactionPolicy = serde_json::from_str(&compaction_json)
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let status = parse_thread_status(&status_str);
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    Ok(AgentThread {
        id,
        agent_id,
        scope,
        compaction_policy,
        context_window_budget: context_window_budget as u32,
        status,
        created_at,
        updated_at,
    })
}

fn parse_thread_status(s: &str) -> ThreadStatus {
    match s {
        "active" => ThreadStatus::Active,
        "completed" => ThreadStatus::Completed,
        "failed" => ThreadStatus::Failed,
        "suspended" => ThreadStatus::Suspended,
        _ => ThreadStatus::Active,
    }
}

fn parse_message_row(row: &sqlx::sqlite::SqliteRow) -> Result<ChatMessage, NeuromancerError> {
    let id_str: String = row
        .try_get("id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let role_str: String = row
        .try_get("role")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let content_json: String = row
        .try_get("content_json")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let token_estimate: i64 = row
        .try_get("token_estimate")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let metadata_json: String = row
        .try_get("metadata_json")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let timestamp_str: String = row
        .try_get("timestamp")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    let id: MessageId = id_str
        .parse()
        .map_err(|e: uuid::Error| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let role: MessageRole = parse_message_role(&role_str);
    let content: MessageContent = serde_json::from_str(&content_json)
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let metadata: MessageMetadata = serde_json::from_str(&metadata_json)
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    Ok(ChatMessage {
        id,
        role,
        content,
        timestamp,
        token_estimate: token_estimate as u32,
        metadata,
    })
}

fn parse_message_role(s: &str) -> MessageRole {
    match s {
        "system" => MessageRole::System,
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        _ => MessageRole::User,
    }
}

fn role_to_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn parse_user_conversation_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<UserConversation, NeuromancerError> {
    let conv_id_str: String = row
        .try_get("conversation_id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let agent_id: String = row
        .try_get("agent_id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let thread_id: String = row
        .try_get("thread_id")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let status_str: String = row
        .try_get("status")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let created_at_str: String = row
        .try_get("created_at")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let updated_at_str: String = row
        .try_get("updated_at")
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    let conversation_id: ConversationId = conv_id_str
        .parse()
        .map_err(|e: uuid::Error| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let status = parse_thread_status(&status_str);
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

    Ok(UserConversation {
        conversation_id,
        agent_id,
        thread_id,
        status,
        created_at,
        updated_at,
    })
}

fn content_preview(content: &MessageContent, max_len: usize) -> String {
    let text = match content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::ToolCalls(calls) => {
            format!("[{} tool call(s)]", calls.len())
        }
        MessageContent::ToolResult(r) => format!("[tool result: {}]", r.call_id),
        MessageContent::Mixed(parts) => format!("[{} parts]", parts.len()),
    };
    if text.len() <= max_len {
        text
    } else {
        format!("{}...", &text[..max_len])
    }
}

// ── ThreadStore trait implementation ─────────────────────────────────────────

#[async_trait::async_trait]
impl neuromancer_core::thread::ThreadStore for SqliteThreadStore {
    async fn create_thread(&self, thread: &AgentThread) -> Result<(), NeuromancerError> {
        let (scope_type, scope_ref, root_scope_type, root_scope_ref) =
            scope_to_columns(&thread.scope);
        let compaction_json = serde_json::to_string(&thread.compaction_policy)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        sqlx::query(
            r#"
            INSERT INTO threads (
                id, agent_id, scope_type, scope_ref, root_scope_type, root_scope_ref,
                compaction_policy_json, context_window_budget, status, created_at, updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&thread.id)
        .bind(&thread.agent_id)
        .bind(&scope_type)
        .bind(&scope_ref)
        .bind(&root_scope_type)
        .bind(&root_scope_ref)
        .bind(&compaction_json)
        .bind(thread.context_window_budget as i64)
        .bind(thread.status.to_string())
        .bind(thread.created_at.to_rfc3339())
        .bind(thread.updated_at.to_rfc3339())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        Ok(())
    }

    async fn get_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<AgentThread>, NeuromancerError> {
        let row = sqlx::query("SELECT * FROM threads WHERE id = ?")
            .bind(thread_id)
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        row.as_ref().map(parse_thread_row).transpose()
    }

    async fn update_status(
        &self,
        thread_id: &ThreadId,
        status: ThreadStatus,
    ) -> Result<(), NeuromancerError> {
        sqlx::query("UPDATE threads SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.to_string())
            .bind(Utc::now().to_rfc3339())
            .bind(thread_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        Ok(())
    }

    async fn list_threads_by_scope_type(
        &self,
        scope_type: &str,
    ) -> Result<Vec<AgentThread>, NeuromancerError> {
        let rows = sqlx::query("SELECT * FROM threads WHERE scope_type = ?")
            .bind(scope_type)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        rows.iter().map(parse_thread_row).collect()
    }

    async fn list_threads_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<AgentThread>, NeuromancerError> {
        let rows = sqlx::query("SELECT * FROM threads WHERE agent_id = ?")
            .bind(agent_id)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        rows.iter().map(parse_thread_row).collect()
    }

    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: &[ChatMessage],
    ) -> Result<(), NeuromancerError> {
        for msg in messages {
            let content_json = serde_json::to_string(&msg.content)
                .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
            let metadata_json = serde_json::to_string(&msg.metadata)
                .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

            sqlx::query(
                r#"
                INSERT INTO thread_messages (
                    id, thread_id, role, content_json, token_estimate,
                    metadata_json, compacted, timestamp
                )
                VALUES (?, ?, ?, ?, ?, ?, 0, ?)
                "#,
            )
            .bind(msg.id.to_string())
            .bind(thread_id)
            .bind(role_to_str(&msg.role))
            .bind(&content_json)
            .bind(msg.token_estimate as i64)
            .bind(&metadata_json)
            .bind(msg.timestamp.to_rfc3339())
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        }
        Ok(())
    }

    async fn load_messages(
        &self,
        thread_id: &ThreadId,
        include_compacted: bool,
    ) -> Result<Vec<ChatMessage>, NeuromancerError> {
        let query = if include_compacted {
            "SELECT * FROM thread_messages WHERE thread_id = ? ORDER BY timestamp ASC"
        } else {
            "SELECT * FROM thread_messages WHERE thread_id = ? AND compacted = 0 ORDER BY timestamp ASC"
        };

        let rows = sqlx::query(query)
            .bind(thread_id)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        rows.iter().map(parse_message_row).collect()
    }

    async fn find_collaboration_thread(
        &self,
        agent_id: &str,
        parent_thread_id: &ThreadId,
    ) -> Result<Option<AgentThread>, NeuromancerError> {
        let row = sqlx::query(
            "SELECT * FROM threads WHERE agent_id = ? AND scope_type = 'collaboration' AND scope_ref = ? AND status = 'active'",
        )
        .bind(agent_id)
        .bind(parent_thread_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        row.as_ref().map(parse_thread_row).transpose()
    }

    async fn resolve_cross_reference(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<CrossReference>, NeuromancerError> {
        let row = sqlx::query(
            "SELECT id, thread_id, role, content_json FROM thread_messages WHERE id = ?",
        )
        .bind(message_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        match row {
            Some(row) => {
                let thread_id: String = row
                    .try_get("thread_id")
                    .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
                let id_str: String = row
                    .try_get("id")
                    .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
                let role: String = row
                    .try_get("role")
                    .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
                let content_json: String = row
                    .try_get("content_json")
                    .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

                let msg_id: MessageId = id_str.parse().map_err(|e: uuid::Error| {
                    NeuromancerError::Infra(InfraError::Database(e.to_string()))
                })?;
                let content: MessageContent = serde_json::from_str(&content_json)
                    .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

                Ok(Some(CrossReference {
                    thread_id,
                    message_id: msg_id,
                    role,
                    content_preview: content_preview(&content, 200),
                }))
            }
            None => Ok(None),
        }
    }

    async fn mark_compacted(
        &self,
        thread_id: &ThreadId,
        up_to_message_id: &MessageId,
    ) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            UPDATE thread_messages SET compacted = 1
            WHERE thread_id = ?
              AND timestamp <= (
                  SELECT timestamp FROM thread_messages WHERE id = ?
              )
            "#,
        )
        .bind(thread_id)
        .bind(up_to_message_id.to_string())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        Ok(())
    }

    async fn total_uncompacted_tokens(
        &self,
        thread_id: &ThreadId,
    ) -> Result<u32, NeuromancerError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(token_estimate), 0) FROM thread_messages WHERE thread_id = ? AND compacted = 0",
        )
        .bind(thread_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        Ok(total as u32)
    }

    async fn find_user_conversation(
        &self,
        agent_id: &str,
    ) -> Result<Option<UserConversation>, NeuromancerError> {
        let row = sqlx::query(
            "SELECT * FROM user_conversations WHERE agent_id = ? AND status = 'active' ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(agent_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        match row {
            Some(row) => Ok(Some(parse_user_conversation_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn save_user_conversation(
        &self,
        conversation: &UserConversation,
    ) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            INSERT INTO user_conversations (conversation_id, agent_id, thread_id, status, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(conversation_id) DO UPDATE SET
                status = excluded.status,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(conversation.conversation_id.to_string())
        .bind(&conversation.agent_id)
        .bind(&conversation.thread_id)
        .bind(conversation.status.to_string())
        .bind(conversation.created_at.to_rfc3339())
        .bind(conversation.updated_at.to_rfc3339())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        Ok(())
    }

    async fn list_user_conversations(
        &self,
    ) -> Result<Vec<UserConversation>, NeuromancerError> {
        let rows = sqlx::query("SELECT * FROM user_conversations ORDER BY updated_at DESC")
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        rows.iter().map(parse_user_conversation_row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::thread::ThreadStore;

    fn sample_thread(id: &str, agent_id: &str, scope: ThreadScope) -> AgentThread {
        let now = Utc::now();
        AgentThread {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            scope,
            compaction_policy: CompactionPolicy::None,
            context_window_budget: 4096,
            status: ThreadStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_message(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    #[tokio::test]
    async fn test_create_and_get_thread() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let thread = sample_thread("t1", "system0", ThreadScope::System0);
        store.create_thread(&thread).await.unwrap();

        let loaded = store.get_thread(&"t1".to_string()).await.unwrap().unwrap();
        assert_eq!(loaded.id, "t1");
        assert_eq!(loaded.agent_id, "system0");
        assert_eq!(loaded.context_window_budget, 4096);
        assert_eq!(loaded.status, ThreadStatus::Active);
        assert_eq!(loaded.scope.scope_type(), "system0");
    }

    #[tokio::test]
    async fn test_update_status() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let thread = sample_thread("t2", "agent-a", ThreadScope::System0);
        store.create_thread(&thread).await.unwrap();

        store
            .update_status(&"t2".to_string(), ThreadStatus::Completed)
            .await
            .unwrap();

        let loaded = store.get_thread(&"t2".to_string()).await.unwrap().unwrap();
        assert_eq!(loaded.status, ThreadStatus::Completed);
    }

    #[tokio::test]
    async fn test_append_and_load_messages() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let thread = sample_thread("t3", "system0", ThreadScope::System0);
        store.create_thread(&thread).await.unwrap();

        let msg1 = sample_message("hello");
        let msg2 = sample_message("world");
        store
            .append_messages(&"t3".to_string(), &[msg1.clone(), msg2.clone()])
            .await
            .unwrap();

        let messages = store
            .load_messages(&"t3".to_string(), false)
            .await
            .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, msg1.id);
        assert_eq!(messages[1].id, msg2.id);
        // Verify content roundtrips
        match &messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn test_load_messages_excludes_compacted() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let thread = sample_thread("t4", "system0", ThreadScope::System0);
        store.create_thread(&thread).await.unwrap();

        let msg1 = sample_message("first");
        let msg2 = sample_message("second");
        let msg3 = sample_message("third");
        store
            .append_messages(
                &"t4".to_string(),
                &[msg1.clone(), msg2.clone(), msg3.clone()],
            )
            .await
            .unwrap();

        // Mark messages up to msg2 as compacted
        store
            .mark_compacted(&"t4".to_string(), &msg2.id)
            .await
            .unwrap();

        // Without compacted: should only get msg3
        let messages = store
            .load_messages(&"t4".to_string(), false)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, msg3.id);

        // With compacted: should get all 3
        let all_messages = store
            .load_messages(&"t4".to_string(), true)
            .await
            .unwrap();
        assert_eq!(all_messages.len(), 3);
    }

    #[tokio::test]
    async fn test_find_collaboration_thread() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let thread = sample_thread(
            "collab-1",
            "agent-b",
            ThreadScope::Collaboration {
                parent_thread_id: "parent-t1".to_string(),
                root_scope: Box::new(ThreadScope::System0),
            },
        );
        store.create_thread(&thread).await.unwrap();

        let found = store
            .find_collaboration_thread("agent-b", &"parent-t1".to_string())
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "collab-1");

        // Different agent should not find it
        let not_found = store
            .find_collaboration_thread("agent-c", &"parent-t1".to_string())
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_list_threads_by_scope_type() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let t1 = sample_thread("s1", "a1", ThreadScope::System0);
        let t2 = sample_thread(
            "s2",
            "a2",
            ThreadScope::Task {
                task_id: "task-1".to_string(),
            },
        );
        let t3 = sample_thread("s3", "a3", ThreadScope::System0);
        store.create_thread(&t1).await.unwrap();
        store.create_thread(&t2).await.unwrap();
        store.create_thread(&t3).await.unwrap();

        let system0_threads = store.list_threads_by_scope_type("system0").await.unwrap();
        assert_eq!(system0_threads.len(), 2);

        let task_threads = store.list_threads_by_scope_type("task").await.unwrap();
        assert_eq!(task_threads.len(), 1);
        assert_eq!(task_threads[0].id, "s2");
    }

    #[tokio::test]
    async fn test_list_threads_for_agent() {
        let store = SqliteThreadStore::in_memory().await.unwrap();
        let t1 = sample_thread("a1-t1", "agent-x", ThreadScope::System0);
        let t2 = sample_thread(
            "a1-t2",
            "agent-x",
            ThreadScope::Task {
                task_id: "task-99".to_string(),
            },
        );
        let t3 = sample_thread("a2-t1", "agent-y", ThreadScope::System0);
        store.create_thread(&t1).await.unwrap();
        store.create_thread(&t2).await.unwrap();
        store.create_thread(&t3).await.unwrap();

        let agent_x_threads = store.list_threads_for_agent("agent-x").await.unwrap();
        assert_eq!(agent_x_threads.len(), 2);

        let agent_y_threads = store.list_threads_for_agent("agent-y").await.unwrap();
        assert_eq!(agent_y_threads.len(), 1);
        assert_eq!(agent_y_threads[0].id, "a2-t1");
    }

    #[tokio::test]
    async fn test_user_conversation_lifecycle() {
        let store = SqliteThreadStore::in_memory().await.unwrap();

        // Create a backing thread first
        let conv_id = uuid::Uuid::new_v4();
        let thread = sample_thread(
            "uc-thread-1",
            "finance-manager",
            ThreadScope::UserConversation {
                conversation_id: conv_id,
            },
        );
        store.create_thread(&thread).await.unwrap();

        // Save user conversation
        let now = Utc::now();
        let conversation = UserConversation {
            conversation_id: conv_id,
            agent_id: "finance-manager".to_string(),
            thread_id: "uc-thread-1".to_string(),
            status: ThreadStatus::Active,
            created_at: now,
            updated_at: now,
        };
        store.save_user_conversation(&conversation).await.unwrap();

        // Find it
        let found = store
            .find_user_conversation("finance-manager")
            .await
            .unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.conversation_id, conv_id);
        assert_eq!(found.thread_id, "uc-thread-1");

        // List all
        let all = store.list_user_conversations().await.unwrap();
        assert_eq!(all.len(), 1);

        // Different agent not found
        let not_found = store
            .find_user_conversation("browser")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }
}
