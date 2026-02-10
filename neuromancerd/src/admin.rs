use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};

use neuromancer_core::config::NeuromancerConfig;

/// Shared application state accessible by all admin API handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<NeuromancerConfig>>,
    pub start_time: Instant,
    pub config_reload_tx: watch::Sender<()>,
}

/// Build the admin API axum router.
pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/admin/health", get(health))
        .route("/admin/tasks", get(list_tasks))
        .route("/admin/tasks/{id}", get(get_task))
        .route("/admin/tasks", post(submit_task))
        .route("/admin/tasks/{id}/cancel", post(cancel_task))
        .route("/admin/agents", get(list_agents))
        .route("/admin/agents/{id}", get(get_agent))
        .route("/admin/cron", get(list_cron))
        .route("/admin/cron/{id}/trigger", post(trigger_cron))
        .route("/admin/cron/{id}/disable", post(disable_cron))
        .route("/admin/cron/{id}/enable", post(enable_cron))
        .route("/admin/memory/stats", get(memory_stats))
        .route("/admin/config/reload", post(reload_config))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_secs: u64,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.start_time.elapsed().as_secs(),
    })
}

// ---------------------------------------------------------------------------
// Tasks (stubs — real task queue will be integrated via orchestrator crate)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TaskSummary {
    id: String,
    instruction: String,
    assigned_agent: String,
    state: String,
    created_at: String,
}

async fn list_tasks(State(_state): State<AppState>) -> Json<Vec<TaskSummary>> {
    // Stub: returns empty list until orchestrator task queue is integrated.
    Json(vec![])
}

async fn get_task(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Stub: no task store yet.
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": format!("task '{id}' not found") })),
    )
}

#[derive(Deserialize)]
struct SubmitTaskRequest {
    instruction: String,
    agent: String,
}

#[derive(Serialize)]
struct SubmitTaskResponse {
    task_id: String,
}

async fn submit_task(
    State(_state): State<AppState>,
    Json(req): Json<SubmitTaskRequest>,
) -> impl IntoResponse {
    // Stub: create a task id but don't actually enqueue (no orchestrator yet).
    let task_id = uuid::Uuid::new_v4();
    tracing::info!(
        task_id = %task_id,
        agent = %req.agent,
        instruction = %req.instruction,
        "manual task submitted (stub)"
    );
    (
        StatusCode::ACCEPTED,
        Json(SubmitTaskResponse {
            task_id: task_id.to_string(),
        }),
    )
}

async fn cancel_task(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(task_id = %id, "cancel task requested (stub)");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "cancel requested", "task_id": id })),
    )
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AgentSummary {
    id: String,
    mode: String,
    status: &'static str,
}

async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let config = state.config.read().await;
    let agents: Vec<AgentSummary> = config
        .agents
        .iter()
        .map(|(id, agent)| AgentSummary {
            id: id.clone(),
            mode: format!("{:?}", agent.mode),
            status: "idle", // Stub — real status from agent registry
        })
        .collect();
    Json(agents)
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    match config.agents.get(&id) {
        Some(agent) => {
            let detail = serde_json::json!({
                "id": id,
                "mode": format!("{:?}", agent.mode),
                "capabilities": {
                    "skills": agent.capabilities.skills,
                    "mcp_servers": agent.capabilities.mcp_servers,
                    "a2a_peers": agent.capabilities.a2a_peers,
                    "memory_partitions": agent.capabilities.memory_partitions,
                },
                "max_iterations": agent.max_iterations,
                "status": "idle",
            });
            (StatusCode::OK, Json(detail))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("agent '{id}' not found") })),
        ),
    }
}

// ---------------------------------------------------------------------------
// Cron
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CronJobSummary {
    id: String,
    description: Option<String>,
    schedule: String,
    enabled: bool,
    agent: String,
}

async fn list_cron(State(state): State<AppState>) -> Json<Vec<CronJobSummary>> {
    let config = state.config.read().await;
    let jobs: Vec<CronJobSummary> = config
        .triggers
        .cron
        .iter()
        .map(|c| CronJobSummary {
            id: c.id.clone(),
            description: c.description.clone(),
            schedule: c.schedule.clone(),
            enabled: c.enabled,
            agent: c.task_template.agent.clone(),
        })
        .collect();
    Json(jobs)
}

async fn trigger_cron(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(cron_id = %id, "manual cron trigger requested (stub)");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "triggered", "cron_id": id })),
    )
}

async fn disable_cron(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(cron_id = %id, "cron disable requested (stub)");
    Json(serde_json::json!({ "status": "disabled", "cron_id": id }))
}

async fn enable_cron(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(cron_id = %id, "cron enable requested (stub)");
    Json(serde_json::json!({ "status": "enabled", "cron_id": id }))
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

async fn memory_stats(State(_state): State<AppState>) -> Json<serde_json::Value> {
    // Stub — will be populated when memory-simple crate is integrated.
    Json(serde_json::json!({
        "total_items": 0,
        "partitions": {},
        "last_gc": null,
    }))
}

// ---------------------------------------------------------------------------
// Config reload
// ---------------------------------------------------------------------------

async fn reload_config(State(state): State<AppState>) -> impl IntoResponse {
    tracing::info!("config reload requested via admin API");
    let _ = state.config_reload_tx.send(());
    Json(serde_json::json!({ "status": "reload triggered" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use neuromancer_core::config::{A2aConfig, GlobalConfig, MemoryConfig, OtelConfig, SecretsConfig, TriggersConfig};
    use neuromancer_core::routing::RoutingConfig;

    fn test_config() -> NeuromancerConfig {
        NeuromancerConfig {
            global: GlobalConfig {
                instance_id: "test-instance".into(),
                workspace_dir: "/tmp".into(),
                data_dir: "/tmp".into(),
            },
            otel: OtelConfig::default(),
            secrets: SecretsConfig::default(),
            memory: MemoryConfig::default(),
            models: HashMap::new(),
            mcp_servers: HashMap::new(),
            a2a: A2aConfig::default(),
            routing: RoutingConfig {
                default_agent: "planner".into(),
                classifier_model: None,
                rules: vec![],
            },
            agents: HashMap::new(),
            triggers: TriggersConfig::default(),
            admin_api: Default::default(),
        }
    }

    #[tokio::test]
    async fn submitted_task_is_visible_in_task_listing() {
        let (reload_tx, _reload_rx) = watch::channel(());
        let state = AppState {
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
            config_reload_tx: reload_tx,
        };

        let _resp = submit_task(
            State(state.clone()),
            Json(SubmitTaskRequest {
                instruction: "run task".into(),
                agent: "planner".into(),
            }),
        )
        .await
        .into_response();

        let Json(tasks) = list_tasks(State(state)).await;

        // Regression guard: manual task submission must be reflected in /admin/tasks.
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].assigned_agent, "planner");
        assert_eq!(tasks[0].instruction, "run task");
    }
}
