use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use neuromancer_core::rpc::{
    ConfigReloadResult, HealthResult, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    OrchestratorContextGetResult, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorRunDiagnoseParams, OrchestratorRunDiagnoseResult, OrchestratorRunGetParams,
    OrchestratorRunGetResult, OrchestratorRunsListResult, OrchestratorStatsGetResult,
    OrchestratorSubagentTurnParams, OrchestratorSubagentTurnResult, OrchestratorThreadGetParams,
    OrchestratorThreadGetResult, OrchestratorThreadResurrectParams,
    OrchestratorThreadResurrectResult, OrchestratorThreadsListResult, OrchestratorTurnParams,
    OrchestratorTurnResult,
};
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum RpcClientError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("rpc error {0}: {1}")]
    Rpc(i64, String),
}

#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    endpoint: String,
    next_id: Arc<AtomicI64>,
}

impl RpcClient {
    pub fn new(addr: &str, timeout: Duration) -> Result<Self, RpcClientError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|err| RpcClientError::Transport(err.to_string()))?;

        let endpoint = format!("{}/rpc", addr.trim_end_matches('/'));

        Ok(Self {
            http,
            endpoint,
            next_id: Arc::new(AtomicI64::new(1)),
        })
    }

    pub async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, RpcClientError> {
        if method.trim().is_empty() {
            return Err(RpcClientError::InvalidRequest(
                "method must not be empty".to_string(),
            ));
        }

        let id = JsonRpcId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: Some(id.clone()),
        };
        let request_debug = serde_json::to_string(&request)
            .unwrap_or_else(|_| format!("{{\"method\":\"{method}\"}}"));

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|err| map_transport_error(err, &self.endpoint, method, &request_debug))?;

        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|err| RpcClientError::Transport(err.to_string()))?;
        let body_debug = String::from_utf8_lossy(&body).to_string();

        let envelope: JsonRpcResponse = serde_json::from_slice(&body).map_err(|err| {
            RpcClientError::InvalidResponse(format!(
                "failed to parse JSON-RPC response for method '{method}': {err}. {}",
                format_rpc_debug_context(&request_debug, &body_debug)
            ))
        })?;

        if envelope.id != Some(id) {
            return Err(RpcClientError::InvalidResponse(format!(
                "mismatched response id for method '{method}'. {}",
                format_rpc_debug_context(&request_debug, &body_debug)
            )));
        }

        if let Some(err) = envelope.error {
            return Err(map_rpc_error(err, &request_debug, &body_debug));
        }

        envelope.result.ok_or_else(|| {
            RpcClientError::InvalidResponse(format!(
                "missing result in response for method '{method}' (http status: {status}). {}",
                format_rpc_debug_context(&request_debug, &body_debug)
            ))
        })
    }

    pub async fn call_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<T, RpcClientError> {
        let result = self.call(method, params).await?;
        serde_json::from_value(result)
            .map_err(|err| RpcClientError::InvalidResponse(err.to_string()))
    }

    pub async fn health(&self) -> Result<HealthResult, RpcClientError> {
        self.call_typed("admin.health", None).await
    }

    pub async fn config_reload(&self) -> Result<ConfigReloadResult, RpcClientError> {
        self.call_typed("admin.config.reload", None).await
    }

    pub async fn orchestrator_turn(
        &self,
        params: OrchestratorTurnParams,
    ) -> Result<OrchestratorTurnResult, RpcClientError> {
        self.call_typed(
            "orchestrator.turn",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_runs_list(
        &self,
    ) -> Result<OrchestratorRunsListResult, RpcClientError> {
        self.call_typed("orchestrator.runs.list", None).await
    }

    pub async fn orchestrator_run_get(
        &self,
        params: OrchestratorRunGetParams,
    ) -> Result<OrchestratorRunGetResult, RpcClientError> {
        self.call_typed(
            "orchestrator.runs.get",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn orchestrator_context_get(
        &self,
    ) -> Result<OrchestratorContextGetResult, RpcClientError> {
        self.call_typed("orchestrator.context.get", None).await
    }

    pub async fn orchestrator_threads_list(
        &self,
    ) -> Result<OrchestratorThreadsListResult, RpcClientError> {
        self.call_typed("orchestrator.threads.list", None).await
    }

    pub async fn orchestrator_thread_get(
        &self,
        params: OrchestratorThreadGetParams,
    ) -> Result<OrchestratorThreadGetResult, RpcClientError> {
        self.call_typed(
            "orchestrator.threads.get",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_thread_resurrect(
        &self,
        params: OrchestratorThreadResurrectParams,
    ) -> Result<OrchestratorThreadResurrectResult, RpcClientError> {
        self.call_typed(
            "orchestrator.threads.resurrect",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_subagent_turn(
        &self,
        params: OrchestratorSubagentTurnParams,
    ) -> Result<OrchestratorSubagentTurnResult, RpcClientError> {
        self.call_typed(
            "orchestrator.subagent.turn",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_events_query(
        &self,
        params: OrchestratorEventsQueryParams,
    ) -> Result<OrchestratorEventsQueryResult, RpcClientError> {
        self.call_typed(
            "orchestrator.events.query",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_run_diagnose(
        &self,
        params: OrchestratorRunDiagnoseParams,
    ) -> Result<OrchestratorRunDiagnoseResult, RpcClientError> {
        self.call_typed(
            "orchestrator.runs.diagnose",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn orchestrator_stats_get(
        &self,
    ) -> Result<OrchestratorStatsGetResult, RpcClientError> {
        self.call_typed("orchestrator.stats.get", None).await
    }
}

fn map_rpc_error(err: JsonRpcError, request_debug: &str, response_debug: &str) -> RpcClientError {
    RpcClientError::Rpc(
        err.code,
        format!(
            "{}. {}",
            err.message,
            format_rpc_debug_context(request_debug, response_debug)
        ),
    )
}

fn map_transport_error(
    err: reqwest::Error,
    endpoint: &str,
    method: &str,
    request_debug: &str,
) -> RpcClientError {
    if err.is_connect() {
        return RpcClientError::Transport(format!(
            "unable to reach daemon admin RPC at '{endpoint}' for method '{method}'. neuromancerd does not appear to be running. Start it with `neuroctl daemon start`. {}",
            format_rpc_debug_context(request_debug, "")
        ));
    }

    if err.is_timeout() {
        return RpcClientError::Transport(format!(
            "request to daemon admin RPC at '{endpoint}' timed out for method '{method}'. {}",
            format_rpc_debug_context(request_debug, "")
        ));
    }

    RpcClientError::Transport(format!(
        "{}. {}",
        err,
        format_rpc_debug_context(request_debug, "")
    ))
}

fn format_rpc_debug_context(request_debug: &str, response_debug: &str) -> String {
    if response_debug.is_empty() {
        return format!("request={}", truncate_debug(request_debug, 4_000));
    }
    format!(
        "request={} response={}",
        truncate_debug(request_debug, 4_000),
        truncate_debug(response_debug, 4_000),
    )
}

fn truncate_debug(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_chars).collect();
    format!("{}...(+{} chars)", truncated, char_count - max_chars)
}
