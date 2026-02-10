use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, watch};

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::rpc::{
    ConfigReloadResult, HealthResult, JSON_RPC_GENERIC_SERVER_ERROR, JSON_RPC_INTERNAL_ERROR,
    JSON_RPC_INVALID_PARAMS, JSON_RPC_INVALID_REQUEST, JSON_RPC_METHOD_NOT_FOUND,
    JSON_RPC_PARSE_ERROR, JSON_RPC_RESOURCE_NOT_FOUND, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    TaskCancelParams, TaskCancelResult, TaskGetParams, TaskGetResult, TaskListResult,
    TaskSubmitParams, TaskSubmitResult, TaskSummaryDto,
};

/// Shared application state accessible by all admin API handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<NeuromancerConfig>>,
    pub start_time: Instant,
    pub config_reload_tx: watch::Sender<()>,
    pub submitted_tasks: Arc<RwLock<HashMap<String, TaskSummary>>>,
}

/// Build the admin API axum router.
pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
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
// Shared admin ops used by REST and RPC surfaces
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct TaskSummary {
    id: String,
    instruction: String,
    assigned_agent: String,
    state: String,
    created_at: String,
}

impl From<&TaskSummary> for TaskSummaryDto {
    fn from(value: &TaskSummary) -> Self {
        Self {
            id: value.id.clone(),
            instruction: value.instruction.clone(),
            assigned_agent: value.assigned_agent.clone(),
            state: value.state.clone(),
            created_at: value.created_at.clone(),
        }
    }
}

async fn op_health(state: &AppState) -> HealthResult {
    HealthResult {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: state.start_time.elapsed().as_secs(),
    }
}

async fn op_list_tasks(state: &AppState) -> TaskListResult {
    let mut tasks: Vec<TaskSummaryDto> = state
        .submitted_tasks
        .read()
        .await
        .values()
        .map(TaskSummaryDto::from)
        .collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    TaskListResult { tasks }
}

async fn op_get_task(state: &AppState, task_id: &str) -> Result<TaskGetResult, RpcMethodError> {
    let tasks = state.submitted_tasks.read().await;
    let Some(task) = tasks.get(task_id) else {
        return Err(RpcMethodError::resource_not_found(format!(
            "task '{}' not found",
            task_id
        )));
    };

    Ok(TaskGetResult {
        task: TaskSummaryDto::from(task),
    })
}

async fn op_submit_task(state: &AppState, req: TaskSubmitParams) -> TaskSubmitResult {
    let task_id = uuid::Uuid::new_v4().to_string();
    let task = TaskSummary {
        id: task_id.clone(),
        instruction: req.instruction.clone(),
        assigned_agent: req.agent.clone(),
        state: "queued".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    state
        .submitted_tasks
        .write()
        .await
        .insert(task.id.clone(), task);

    tracing::info!(
        task_id = %task_id,
        agent = %req.agent,
        instruction = %req.instruction,
        "manual task submitted"
    );

    TaskSubmitResult { task_id }
}

async fn op_cancel_task(state: &AppState, task_id: &str) -> TaskCancelResult {
    let mut tasks = state.submitted_tasks.write().await;
    if let Some(task) = tasks.get_mut(task_id) {
        task.state = "cancelled".to_string();
    }

    tracing::info!(task_id = %task_id, "cancel task requested");

    TaskCancelResult {
        status: "cancel requested".to_string(),
        task_id: task_id.to_string(),
    }
}

fn op_reload_config(state: &AppState) -> Result<ConfigReloadResult, RpcMethodError> {
    tracing::info!("config reload requested via admin API");
    state
        .config_reload_tx
        .send(())
        .map_err(|e| RpcMethodError::internal(format!("failed to trigger config reload: {e}")))?;

    Ok(ConfigReloadResult {
        status: "reload triggered".to_string(),
    })
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 dispatcher (/rpc)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RpcMethodError {
    code: i64,
    message: String,
}

impl RpcMethodError {
    fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_METHOD_NOT_FOUND,
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_INVALID_PARAMS,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_INTERNAL_ERROR,
            message: message.into(),
        }
    }

    fn resource_not_found(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_RESOURCE_NOT_FOUND,
            message: message.into(),
        }
    }

    fn generic(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_GENERIC_SERVER_ERROR,
            message: message.into(),
        }
    }
}

async fn rpc_handler(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
    let (status, response) = handle_rpc_bytes(&state, &body).await;
    (status, Json(response))
}

async fn handle_rpc_bytes(state: &AppState, body: &[u8]) -> (StatusCode, JsonRpcResponse) {
    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                JsonRpcResponse::error(None, JSON_RPC_PARSE_ERROR, "Parse error"),
            );
        }
    };

    if payload.is_array() {
        return (
            StatusCode::OK,
            JsonRpcResponse::error(
                None,
                JSON_RPC_INVALID_REQUEST,
                "Batch requests are not supported",
            ),
        );
    }

    let req: JsonRpcRequest = match serde_json::from_value(payload) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::OK,
                JsonRpcResponse::error(None, JSON_RPC_INVALID_REQUEST, "Invalid request"),
            );
        }
    };

    if req.jsonrpc != "2.0" || req.method.trim().is_empty() {
        return (
            StatusCode::OK,
            JsonRpcResponse::error(req.id, JSON_RPC_INVALID_REQUEST, "Invalid request"),
        );
    }

    let Some(id) = req.id.clone() else {
        return (
            StatusCode::OK,
            JsonRpcResponse::error(
                None,
                JSON_RPC_INVALID_REQUEST,
                "Notifications are not supported",
            ),
        );
    };

    let response = dispatch_rpc(state, id, &req.method, req.params).await;
    (StatusCode::OK, response)
}

async fn dispatch_rpc(
    state: &AppState,
    id: JsonRpcId,
    method: &str,
    params: Option<serde_json::Value>,
) -> JsonRpcResponse {
    let result: Result<serde_json::Value, RpcMethodError> = match method {
        "admin.health" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                let health = op_health(state).await;
                to_value(&health)
            }
        }
        "task.submit" => match parse_params::<TaskSubmitParams>(params) {
            Ok(req) => {
                let submitted = op_submit_task(state, req).await;
                to_value(&submitted)
            }
            Err(err) => Err(err),
        },
        "task.list" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                let listed = op_list_tasks(state).await;
                to_value(&listed)
            }
        }
        "task.get" => match parse_params::<TaskGetParams>(params) {
            Ok(req) => match op_get_task(state, &req.task_id).await {
                Ok(result) => to_value(&result),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        "task.cancel" => match parse_params::<TaskCancelParams>(params) {
            Ok(req) => {
                let cancelled = op_cancel_task(state, &req.task_id).await;
                to_value(&cancelled)
            }
            Err(err) => Err(err),
        },
        "admin.config.reload" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                match op_reload_config(state) {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                }
            }
        }
        _ => Err(RpcMethodError::method_not_found(format!(
            "Method '{}' not found",
            method
        ))),
    };

    match result {
        Ok(result) => JsonRpcResponse::success(id, result),
        Err(err) => JsonRpcResponse::error(Some(id), err.code, err.message),
    }
}

fn parse_params<T>(params: Option<serde_json::Value>) -> Result<T, RpcMethodError>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = params else {
        return Err(RpcMethodError::invalid_params("params are required"));
    };

    serde_json::from_value(value).map_err(|e| RpcMethodError::invalid_params(e.to_string()))
}

fn require_no_params_or_empty_object(
    params: Option<&serde_json::Value>,
) -> Result<(), RpcMethodError> {
    match params {
        None => Ok(()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(()),
        Some(_) => Err(RpcMethodError::invalid_params(
            "method does not accept params",
        )),
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<serde_json::Value, RpcMethodError> {
    serde_json::to_value(value)
        .map_err(|e| RpcMethodError::generic(format!("serialization error: {e}")))
}

// ---------------------------------------------------------------------------
// REST handlers
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Json<HealthResult> {
    Json(op_health(&state).await)
}

async fn list_tasks(State(state): State<AppState>) -> Json<Vec<TaskSummaryDto>> {
    Json(op_list_tasks(&state).await.tasks)
}

async fn get_task(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match op_get_task(&state, &id).await {
        Ok(task) => (StatusCode::OK, Json(serde_json::json!(task.task))),
        Err(err) if err.code == JSON_RPC_RESOURCE_NOT_FOUND => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": err.message })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.message })),
        ),
    }
}

async fn submit_task(
    State(state): State<AppState>,
    Json(req): Json<TaskSubmitParams>,
) -> impl IntoResponse {
    let submitted = op_submit_task(&state, req).await;
    (StatusCode::ACCEPTED, Json(submitted))
}

async fn cancel_task(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let cancelled = op_cancel_task(&state, &id).await;
    (StatusCode::ACCEPTED, Json(cancelled))
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

async fn get_agent(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
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
            Json(serde_json::json!({ "error": format!("agent '{}' not found", id) })),
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

async fn trigger_cron(State(_state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    tracing::info!(cron_id = %id, "manual cron trigger requested (stub)");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "triggered", "cron_id": id })),
    )
}

async fn disable_cron(State(_state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    tracing::info!(cron_id = %id, "cron disable requested (stub)");
    Json(serde_json::json!({ "status": "disabled", "cron_id": id }))
}

async fn enable_cron(State(_state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
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
    match op_reload_config(&state) {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.message })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use neuromancer_core::config::{
        A2aConfig, GlobalConfig, MemoryConfig, OtelConfig, SecretsConfig, TriggersConfig,
    };
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

    fn test_state() -> AppState {
        let (reload_tx, _reload_rx) = watch::channel(());
        AppState {
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
            config_reload_tx: reload_tx,
            submitted_tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn rpc_json(
        state: &AppState,
        payload: serde_json::Value,
    ) -> (StatusCode, JsonRpcResponse) {
        let body = serde_json::to_vec(&payload).expect("payload should encode");
        handle_rpc_bytes(state, &body).await
    }

    #[tokio::test]
    async fn submitted_task_is_visible_in_task_listing() {
        let state = test_state();

        let _resp = submit_task(
            State(state.clone()),
            Json(TaskSubmitParams {
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

    #[tokio::test]
    async fn rpc_health_returns_success_envelope() {
        let state = test_state();
        let (status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "admin.health"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.id, Some(JsonRpcId::Number(1)));
        assert!(response.error.is_none());

        let health: HealthResult = serde_json::from_value(response.result.expect("health result"))
            .expect("valid health result");
        assert_eq!(health.status, "ok");
    }

    #[tokio::test]
    async fn rpc_returns_parse_error_for_malformed_body() {
        let state = test_state();
        let (status, response) = handle_rpc_bytes(&state, b"{not-json").await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response.error.expect("error").code, JSON_RPC_PARSE_ERROR);
    }

    #[tokio::test]
    async fn rpc_rejects_invalid_request_envelope() {
        let state = test_state();
        let (status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "1.0",
                "id": 1,
                "method": "admin.health"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.error.expect("error").code,
            JSON_RPC_INVALID_REQUEST
        );
    }

    #[tokio::test]
    async fn rpc_rejects_notifications() {
        let state = test_state();
        let (status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin.health"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.error.expect("error").code,
            JSON_RPC_INVALID_REQUEST
        );
    }

    #[tokio::test]
    async fn rpc_rejects_batch_requests() {
        let state = test_state();
        let (status, response) = rpc_json(
            &state,
            serde_json::json!([
                {"jsonrpc":"2.0", "id": 1, "method": "admin.health"}
            ]),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.error.expect("error").code,
            JSON_RPC_INVALID_REQUEST
        );
    }

    #[tokio::test]
    async fn rpc_returns_method_not_found() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "nope"
            }),
        )
        .await;

        assert_eq!(
            response.error.expect("error").code,
            JSON_RPC_METHOD_NOT_FOUND
        );
    }

    #[tokio::test]
    async fn rpc_returns_invalid_params() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "task.submit",
                "params": {"instruction": "x"}
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn rpc_returns_resource_not_found_for_missing_task() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "task.get",
                "params": {"task_id": "missing"}
            }),
        )
        .await;

        assert_eq!(
            response.error.expect("error").code,
            JSON_RPC_RESOURCE_NOT_FOUND
        );
    }

    #[tokio::test]
    async fn rpc_task_lifecycle_roundtrip() {
        let state = test_state();

        let (_status, submit_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "task.submit",
                "params": {"instruction": "smoke", "agent": "planner"}
            }),
        )
        .await;

        let submit: TaskSubmitResult = serde_json::from_value(
            submit_response
                .result
                .expect("submit response should include result"),
        )
        .expect("valid submit result");

        let (_status, list_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "task.list"
            }),
        )
        .await;

        let list: TaskListResult =
            serde_json::from_value(list_response.result.expect("list result"))
                .expect("valid list result");
        assert!(list.tasks.iter().any(|task| task.id == submit.task_id));

        let (_status, get_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "task.get",
                "params": {"task_id": submit.task_id}
            }),
        )
        .await;

        let get: TaskGetResult = serde_json::from_value(get_response.result.expect("get result"))
            .expect("valid get result");
        assert_eq!(get.task.state, "queued");

        let (_status, _cancel_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "task.cancel",
                "params": {"task_id": get.task.id}
            }),
        )
        .await;

        let (_status, get_after_cancel_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "task.get",
                "params": {"task_id": get.task.id}
            }),
        )
        .await;

        let after_cancel: TaskGetResult = serde_json::from_value(
            get_after_cancel_response
                .result
                .expect("post-cancel get result"),
        )
        .expect("valid post-cancel get result");
        assert_eq!(after_cancel.task.state, "cancelled");
    }
}
