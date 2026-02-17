use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use neuromancer_core::agent::TaskExecutionState;
use neuromancer_core::error::{InfraError, NeuromancerError};
use neuromancer_core::task::{Task, TaskId, TaskState};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{SqlitePool, prelude::FromRow};
use std::str::FromStr;

#[derive(Clone)]
pub(crate) struct TaskStore {
    pool: Arc<SqlitePool>,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedCheckpoint {
    pub task_id: TaskId,
    pub created_at: DateTime<Utc>,
    pub execution_state: TaskExecutionState,
    pub conversation_json: Option<serde_json::Value>,
    pub reason: Option<String>,
}

#[derive(Debug, FromRow)]
struct TaskRow {
    task_json: String,
}

#[derive(Debug, FromRow)]
struct CheckpointRow {
    task_id: String,
    created_at: String,
    execution_state_json: String,
    conversation_json: Option<String>,
    reason: Option<String>,
}

impl TaskStore {
    pub(crate) async fn open(path: &Path) -> Result<Self, NeuromancerError> {
        let parent = path.parent().ok_or_else(|| {
            NeuromancerError::Infra(InfraError::Config(format!(
                "invalid task db path '{}'",
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
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                trigger_source TEXT NOT NULL,
                instruction TEXT NOT NULL,
                assigned_agent TEXT NOT NULL,
                state TEXT NOT NULL,
                priority INTEGER NOT NULL,
                deadline TEXT,
                idempotency_key TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                task_json TEXT NOT NULL
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_tasks_state
            ON tasks(state)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_tasks_priority_created
            ON tasks(priority DESC, created_at ASC)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_tasks_assigned_state
            ON tasks(assigned_agent, state)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_tasks_idempotency_state
            ON tasks(idempotency_key, state)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS task_checkpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                execution_state_json TEXT NOT NULL,
                conversation_json TEXT,
                reason TEXT
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_task_checkpoints_task_created
            ON task_checkpoints(task_id, created_at DESC)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS task_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                from_state TEXT NOT NULL,
                to_state TEXT NOT NULL,
                created_at TEXT NOT NULL,
                metadata_json TEXT
            )
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        Ok(())
    }

    pub(crate) async fn upsert_task(&self, task: &Task) -> Result<(), NeuromancerError> {
        let task_json = serde_json::to_string(task)
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        let parent_id = task.parent_id.map(|id| id.to_string());
        let trigger_source = serde_json::to_string(&task.trigger_source)
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        let trigger_source = trigger_source.trim_matches('"').to_string();
        let state_label = task_state_label(&task.state);
        let priority = task.priority as i64;
        let deadline = task.deadline.map(|dt| dt.to_rfc3339());

        sqlx::query(
            r#"
            INSERT INTO tasks (
                id, parent_id, trigger_source, instruction, assigned_agent, state, priority,
                deadline, idempotency_key, created_at, updated_at, task_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                trigger_source = excluded.trigger_source,
                instruction = excluded.instruction,
                assigned_agent = excluded.assigned_agent,
                state = excluded.state,
                priority = excluded.priority,
                deadline = excluded.deadline,
                idempotency_key = excluded.idempotency_key,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                task_json = excluded.task_json
            "#,
        )
        .bind(task.id.to_string())
        .bind(parent_id)
        .bind(trigger_source)
        .bind(&task.instruction)
        .bind(&task.assigned_agent)
        .bind(state_label)
        .bind(priority)
        .bind(deadline)
        .bind(task.idempotency_key.clone())
        .bind(task.created_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .bind(task_json)
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        Ok(())
    }

    pub(crate) async fn get_task(&self, task_id: TaskId) -> Result<Option<Task>, NeuromancerError> {
        let row = sqlx::query_as::<_, TaskRow>(
            r#"
            SELECT task_json
            FROM tasks
            WHERE id = ?
            "#,
        )
        .bind(task_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        row.map(|row| {
            serde_json::from_str::<Task>(&row.task_json)
                .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))
        })
        .transpose()
    }

    pub(crate) async fn list_tasks_by_states(
        &self,
        states: &[&str],
    ) -> Result<Vec<Task>, NeuromancerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = std::iter::repeat_n("?", states.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT task_json FROM tasks WHERE state IN ({placeholders}) ORDER BY created_at ASC"
        );
        let mut sql = sqlx::query_as::<_, TaskRow>(&query);
        for state in states {
            sql = sql.bind(*state);
        }
        let rows = sql
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        rows.into_iter()
            .map(|row| {
                serde_json::from_str::<Task>(&row.task_json)
                    .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))
            })
            .collect()
    }

    pub(crate) async fn find_non_terminal_by_idempotency(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<TaskId>, NeuromancerError> {
        let task_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT id
            FROM tasks
            WHERE idempotency_key = ?
              AND state IN ('queued', 'dispatched', 'running')
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(idempotency_key)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        task_id
            .map(|id| {
                id.parse::<TaskId>()
                    .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))
            })
            .transpose()
    }

    pub(crate) async fn record_transition(
        &self,
        task_id: TaskId,
        from_state: &str,
        to_state: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            INSERT INTO task_events (task_id, from_state, to_state, created_at, metadata_json)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(task_id.to_string())
        .bind(from_state)
        .bind(to_state)
        .bind(Utc::now().to_rfc3339())
        .bind(metadata.map(|value| value.to_string()))
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        Ok(())
    }

    pub(crate) async fn save_checkpoint(
        &self,
        checkpoint: &PersistedCheckpoint,
    ) -> Result<(), NeuromancerError> {
        let execution_state_json = serde_json::to_string(&checkpoint.execution_state)
            .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
        let conversation_json = checkpoint
            .conversation_json
            .as_ref()
            .map(serde_json::Value::to_string);

        sqlx::query(
            r#"
            INSERT INTO task_checkpoints (
                task_id, created_at, execution_state_json, conversation_json, reason
            )
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(checkpoint.task_id.to_string())
        .bind(checkpoint.created_at.to_rfc3339())
        .bind(execution_state_json)
        .bind(conversation_json)
        .bind(checkpoint.reason.clone())
        .execute(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        Ok(())
    }

    pub(crate) async fn latest_checkpoint(
        &self,
        task_id: TaskId,
    ) -> Result<Option<PersistedCheckpoint>, NeuromancerError> {
        let row = sqlx::query_as::<_, CheckpointRow>(
            r#"
            SELECT task_id, created_at, execution_state_json, conversation_json, reason
            FROM task_checkpoints
            WHERE task_id = ?
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(task_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;

        row.map(|row| parse_checkpoint_row(row)).transpose()
    }
}

pub(crate) fn task_state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Dispatched => "dispatched",
        TaskState::Running { .. } => "running",
        TaskState::Completed { .. } => "completed",
        TaskState::Failed { .. } => "failed",
        TaskState::Cancelled { .. } => "cancelled",
    }
}

fn parse_checkpoint_row(row: CheckpointRow) -> Result<PersistedCheckpoint, NeuromancerError> {
    let task_id = row
        .task_id
        .parse::<TaskId>()
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
    let created_at = DateTime::parse_from_rfc3339(&row.created_at)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
    let execution_state = serde_json::from_str::<TaskExecutionState>(&row.execution_state_json)
        .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))?;
    let conversation_json = row
        .conversation_json
        .as_ref()
        .map(|json| {
            serde_json::from_str::<serde_json::Value>(json)
                .map_err(|err| NeuromancerError::Infra(InfraError::Database(err.to_string())))
        })
        .transpose()?;

    Ok(PersistedCheckpoint {
        task_id,
        created_at,
        execution_state,
        conversation_json,
        reason: row.reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::task::{TaskPriority, TokenUsage};
    use neuromancer_core::trigger::TriggerSource;

    fn sample_task() -> Task {
        let now = Utc::now();
        Task {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            trigger_source: TriggerSource::Internal,
            instruction: "Do a thing".to_string(),
            assigned_agent: "planner".to_string(),
            state: TaskState::Queued,
            priority: TaskPriority::High,
            deadline: None,
            checkpoints: Vec::new(),
            idempotency_key: Some("same-work".to_string()),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn migrate_and_roundtrip_task() {
        let store = TaskStore::in_memory().await.expect("store");
        let task = sample_task();
        store.upsert_task(&task).await.expect("upsert");
        let loaded = store
            .get_task(task.id)
            .await
            .expect("get")
            .expect("task present");
        assert_eq!(loaded.id, task.id);
        assert_eq!(loaded.instruction, "Do a thing");
    }

    #[tokio::test]
    async fn idempotency_lookup_returns_non_terminal_task() {
        let store = TaskStore::in_memory().await.expect("store");
        let task = sample_task();
        let task_id = task.id;
        store.upsert_task(&task).await.expect("upsert");
        let existing = store
            .find_non_terminal_by_idempotency("same-work")
            .await
            .expect("lookup")
            .expect("existing");
        assert_eq!(existing, task_id);
    }

    #[tokio::test]
    async fn transition_event_is_persisted() {
        let store = TaskStore::in_memory().await.expect("store");
        let task = sample_task();
        store.upsert_task(&task).await.expect("upsert");
        store
            .record_transition(
                task.id,
                "queued",
                "running",
                Some(serde_json::json!({"source": "unit_test"})),
            )
            .await
            .expect("record transition");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_events WHERE task_id = ? AND from_state = ? AND to_state = ?",
        )
        .bind(task.id.to_string())
        .bind("queued")
        .bind("running")
        .fetch_one(store.pool.as_ref())
        .await
        .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn checkpoint_roundtrip() {
        let store = TaskStore::in_memory().await.expect("store");
        let mut task = sample_task();
        task.state = TaskState::Completed {
            output: neuromancer_core::task::TaskOutput {
                artifacts: vec![],
                summary: "done".to_string(),
                token_usage: TokenUsage::default(),
                duration: std::time::Duration::from_millis(1),
            },
        };
        store.upsert_task(&task).await.expect("upsert");
        let checkpoint = PersistedCheckpoint {
            task_id: task.id,
            created_at: Utc::now(),
            execution_state: TaskExecutionState::Completed {
                output: match task.state.clone() {
                    TaskState::Completed { output } => output,
                    _ => unreachable!("completed state expected"),
                },
            },
            conversation_json: Some(serde_json::json!({"messages": []})),
            reason: Some("unit-test".to_string()),
        };
        store
            .save_checkpoint(&checkpoint)
            .await
            .expect("checkpoint");
        let loaded = store
            .latest_checkpoint(task.id)
            .await
            .expect("load checkpoint")
            .expect("checkpoint exists");
        assert_eq!(loaded.task_id, task.id);
        assert_eq!(loaded.reason.as_deref(), Some("unit-test"));
    }
}
