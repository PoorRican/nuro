//! RPC endpoints for neuromancerd
//! 
//! Only used for CLI. Incoming inputs from 
// TODO: file should likely be renamed to `rpc.rs` to reflect purpose.
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;

use crate::orchestrator::{OrchestratorRuntime, OrchestratorRuntimeError};
use neuromancer_core::rpc::{
    ConfigReloadResult, HealthResult, JSON_RPC_GENERIC_SERVER_ERROR, JSON_RPC_INTERNAL_ERROR,
    JSON_RPC_INVALID_PARAMS, JSON_RPC_INVALID_REQUEST, JSON_RPC_METHOD_NOT_FOUND,
    JSON_RPC_PARSE_ERROR, JSON_RPC_RESOURCE_NOT_FOUND, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    OrchestratorContextGetResult, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorOutputsPullParams, OrchestratorOutputsPullResult, OrchestratorRunDiagnoseParams,
    OrchestratorRunDiagnoseResult, OrchestratorRunGetParams, OrchestratorRunGetResult,
    OrchestratorRunsListResult, OrchestratorStatsGetResult, OrchestratorSubagentTurnParams,
    OrchestratorSubagentTurnResult, OrchestratorThreadGetParams, OrchestratorThreadGetResult,
    OrchestratorThreadResurrectParams, OrchestratorThreadResurrectResult,
    OrchestratorThreadsListResult, OrchestratorTurnParams, OrchestratorTurnResult,
};

#[derive(Clone)]
pub struct AppState {
    pub start_time: Instant,
    pub config_reload_tx: watch::Sender<()>,
    pub orchestrator_runtime: Option<Arc<OrchestratorRuntime>>,
}

pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/admin/health", get(health))
        .route("/admin/config/reload", post(reload_config))
        .with_state(state)
}

async fn op_health(state: &AppState) -> HealthResult {
    HealthResult {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: state.start_time.elapsed().as_secs(),
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

async fn op_orchestrator_turn(
    state: &AppState,
    params: OrchestratorTurnParams,
) -> Result<OrchestratorTurnResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_turn(params.message)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_runs_list(
    state: &AppState,
) -> Result<OrchestratorRunsListResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    let runs = runtime
        .orchestrator_runs_list()
        .await
        .map_err(map_orchestrator_runtime_error)?;
    Ok(OrchestratorRunsListResult { runs })
}

async fn op_orchestrator_outputs_pull(
    state: &AppState,
    params: OrchestratorOutputsPullParams,
) -> Result<OrchestratorOutputsPullResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_outputs_pull(params.limit)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_run_get(
    state: &AppState,
    params: OrchestratorRunGetParams,
) -> Result<OrchestratorRunGetResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    let run = runtime
        .orchestrator_run_get(params.run_id)
        .await
        .map_err(map_orchestrator_runtime_error)?;
    Ok(OrchestratorRunGetResult { run })
}

async fn op_orchestrator_context_get(
    state: &AppState,
) -> Result<OrchestratorContextGetResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    let messages = runtime
        .orchestrator_context_get()
        .await
        .map_err(map_orchestrator_runtime_error)?;
    Ok(OrchestratorContextGetResult { messages })
}

async fn op_orchestrator_threads_list(
    state: &AppState,
) -> Result<OrchestratorThreadsListResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    let threads = runtime
        .orchestrator_threads_list()
        .await
        .map_err(map_orchestrator_runtime_error)?;
    Ok(OrchestratorThreadsListResult { threads })
}

async fn op_orchestrator_thread_get(
    state: &AppState,
    params: OrchestratorThreadGetParams,
) -> Result<OrchestratorThreadGetResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_thread_get(params)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_thread_resurrect(
    state: &AppState,
    params: OrchestratorThreadResurrectParams,
) -> Result<OrchestratorThreadResurrectResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_thread_resurrect(params.thread_id)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_subagent_turn(
    state: &AppState,
    params: OrchestratorSubagentTurnParams,
) -> Result<OrchestratorSubagentTurnResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_subagent_turn(params.thread_id, params.message)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_events_query(
    state: &AppState,
    params: OrchestratorEventsQueryParams,
) -> Result<OrchestratorEventsQueryResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_events_query(params)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_run_diagnose(
    state: &AppState,
    params: OrchestratorRunDiagnoseParams,
) -> Result<OrchestratorRunDiagnoseResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_run_diagnose(params.run_id)
        .await
        .map_err(map_orchestrator_runtime_error)
}

async fn op_orchestrator_stats_get(
    state: &AppState,
) -> Result<OrchestratorStatsGetResult, RpcMethodError> {
    let Some(runtime) = &state.orchestrator_runtime else {
        return Err(RpcMethodError::internal(
            "orchestrator runtime is not initialized".to_string(),
        ));
    };

    runtime
        .orchestrator_stats_get()
        .await
        .map_err(map_orchestrator_runtime_error)
}

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
        "orchestrator.turn" => match parse_params::<OrchestratorTurnParams>(params) {
            Ok(req) => match op_orchestrator_turn(state, req).await {
                Ok(result) => to_value(&result),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        "orchestrator.runs.list" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                match op_orchestrator_runs_list(state).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                }
            }
        }
        "orchestrator.outputs.pull" => {
            match parse_params::<OrchestratorOutputsPullParams>(params) {
                Ok(req) => match op_orchestrator_outputs_pull(state, req).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            }
        }
        "orchestrator.runs.get" => match parse_params::<OrchestratorRunGetParams>(params) {
            Ok(req) => match op_orchestrator_run_get(state, req).await {
                Ok(result) => to_value(&result),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        "orchestrator.context.get" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                match op_orchestrator_context_get(state).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                }
            }
        }
        "orchestrator.threads.list" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                match op_orchestrator_threads_list(state).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                }
            }
        }
        "orchestrator.threads.get" => match parse_params::<OrchestratorThreadGetParams>(params) {
            Ok(req) => match op_orchestrator_thread_get(state, req).await {
                Ok(result) => to_value(&result),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        "orchestrator.threads.resurrect" => {
            match parse_params::<OrchestratorThreadResurrectParams>(params) {
                Ok(req) => match op_orchestrator_thread_resurrect(state, req).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            }
        }
        "orchestrator.subagent.turn" => {
            match parse_params::<OrchestratorSubagentTurnParams>(params) {
                Ok(req) => match op_orchestrator_subagent_turn(state, req).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            }
        }
        "orchestrator.events.query" => {
            match parse_params::<OrchestratorEventsQueryParams>(params) {
                Ok(req) => match op_orchestrator_events_query(state, req).await {
                    Ok(result) => to_value(&result),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            }
        }
        "orchestrator.runs.diagnose" => match parse_params::<OrchestratorRunDiagnoseParams>(params)
        {
            Ok(req) => match op_orchestrator_run_diagnose(state, req).await {
                Ok(result) => to_value(&result),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        "orchestrator.stats.get" => {
            if let Err(err) = require_no_params_or_empty_object(params.as_ref()) {
                Err(err)
            } else {
                match op_orchestrator_stats_get(state).await {
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

fn map_orchestrator_runtime_error(err: OrchestratorRuntimeError) -> RpcMethodError {
    if err.is_invalid_request() {
        return RpcMethodError::invalid_params(err.to_string());
    }
    if err.is_resource_not_found() {
        return RpcMethodError::resource_not_found(err.to_string());
    }

    RpcMethodError::internal(err.to_string())
}

async fn health(State(state): State<AppState>) -> Json<HealthResult> {
    Json(op_health(&state).await)
}

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

    fn test_state() -> AppState {
        let (reload_tx, _reload_rx) = watch::channel(());
        AppState {
            start_time: Instant::now(),
            config_reload_tx: reload_tx,
            orchestrator_runtime: None,
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
    }

    #[tokio::test]
    async fn rpc_orchestrator_turn_requires_runtime() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "orchestrator.turn",
                "params": {"message": "hello"}
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn rpc_orchestrator_runs_list_requires_runtime() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 13,
                "method": "orchestrator.runs.list"
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn rpc_orchestrator_outputs_pull_requires_runtime() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1337,
                "method": "orchestrator.outputs.pull",
                "params": {"limit": 5}
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn rpc_orchestrator_runs_get_requires_run_id_param() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 14,
                "method": "orchestrator.runs.get",
                "params": {}
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn rpc_orchestrator_context_get_requires_runtime() {
        let state = test_state();
        let (_status, response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 15,
                "method": "orchestrator.context.get"
            }),
        )
        .await;

        assert_eq!(response.error.expect("error").code, JSON_RPC_INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn removed_legacy_methods_return_method_not_found() {
        let state = test_state();
        let (_status, task_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "task.submit",
                "params": {"instruction": "x", "agent": "y"}
            }),
        )
        .await;

        assert_eq!(
            task_response.error.expect("error").code,
            JSON_RPC_METHOD_NOT_FOUND
        );

        let (_status, message_response) = rpc_json(
            &state,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "message.send",
                "params": {"message": "legacy"}
            }),
        )
        .await;

        assert_eq!(
            message_response.error.expect("error").code,
            JSON_RPC_METHOD_NOT_FOUND
        );
    }
}
