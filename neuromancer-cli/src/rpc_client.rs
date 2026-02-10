use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use neuromancer_core::rpc::{
    ConfigReloadResult, HealthResult, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    TaskCancelParams, TaskCancelResult, TaskGetParams, TaskGetResult, TaskListResult,
    TaskSubmitParams, TaskSubmitResult,
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

pub struct RpcClient {
    http: reqwest::Client,
    endpoint: String,
    next_id: AtomicI64,
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
            next_id: AtomicI64::new(1),
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

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|err| RpcClientError::Transport(err.to_string()))?;

        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|err| RpcClientError::Transport(err.to_string()))?;

        let envelope: JsonRpcResponse = serde_json::from_slice(&body)
            .map_err(|err| RpcClientError::InvalidResponse(err.to_string()))?;

        if envelope.id != Some(id) {
            return Err(RpcClientError::InvalidResponse(format!(
                "mismatched response id for method '{method}'"
            )));
        }

        if let Some(err) = envelope.error {
            return Err(map_rpc_error(err));
        }

        envelope.result.ok_or_else(|| {
            RpcClientError::InvalidResponse(format!(
                "missing result in response for method '{method}' (http status: {status})"
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

    pub async fn task_submit(
        &self,
        params: TaskSubmitParams,
    ) -> Result<TaskSubmitResult, RpcClientError> {
        self.call_typed(
            "task.submit",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn task_list(&self) -> Result<TaskListResult, RpcClientError> {
        self.call_typed("task.list", None).await
    }

    pub async fn task_get(&self, params: TaskGetParams) -> Result<TaskGetResult, RpcClientError> {
        self.call_typed(
            "task.get",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn task_cancel(
        &self,
        params: TaskCancelParams,
    ) -> Result<TaskCancelResult, RpcClientError> {
        self.call_typed(
            "task.cancel",
            Some(
                serde_json::to_value(params)
                    .map_err(|err| RpcClientError::InvalidRequest(err.to_string()))?,
            ),
        )
        .await
    }

    pub async fn config_reload(&self) -> Result<ConfigReloadResult, RpcClientError> {
        self.call_typed("admin.config.reload", None).await
    }
}

fn map_rpc_error(err: JsonRpcError) -> RpcClientError {
    RpcClientError::Rpc(err.code, err.message)
}
