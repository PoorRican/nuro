use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 id type supported by the daemon control-plane RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
}

impl JsonRpcResponse {
    pub fn success(id: JsonRpcId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id: Some(id),
        }
    }

    pub fn error(id: Option<JsonRpcId>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
            id,
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

pub const JSON_RPC_PARSE_ERROR: i64 = -32700;
pub const JSON_RPC_INVALID_REQUEST: i64 = -32600;
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSON_RPC_INVALID_PARAMS: i64 = -32602;
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
pub const JSON_RPC_GENERIC_SERVER_ERROR: i64 = -32000;
pub const JSON_RPC_RESOURCE_NOT_FOUND: i64 = -32004;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResult {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloadResult {
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorTurnParams {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegatedRun {
    pub run_id: String,
    pub agent_id: String,
    pub state: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorToolInvocation {
    pub call_id: String,
    pub tool_id: String,
    pub arguments: serde_json::Value,
    pub status: String,
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorTurnResult {
    pub turn_id: String,
    pub response: String,
    pub delegated_runs: Vec<DelegatedRun>,
    pub tool_invocations: Vec<OrchestratorToolInvocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorRunsListResult {
    pub runs: Vec<DelegatedRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorRunGetParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorRunGetResult {
    pub run: DelegatedRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorThreadMessage {
    Text {
        role: String,
        content: String,
    },
    ToolInvocation {
        call_id: String,
        tool_id: String,
        arguments: serde_json::Value,
        status: String,
        output: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorContextGetResult {
    pub messages: Vec<OrchestratorThreadMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_request_roundtrip_supports_string_id() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "orchestrator.turn".to_string(),
            params: Some(serde_json::json!({ "message": "hello" })),
            id: Some(JsonRpcId::String("req-1".to_string())),
        };

        let encoded = serde_json::to_string(&req).expect("request should serialize");
        let decoded: JsonRpcRequest =
            serde_json::from_str(&encoded).expect("request should deserialize");

        assert_eq!(decoded, req);
    }

    #[test]
    fn jsonrpc_response_roundtrip_supports_numeric_id() {
        let resp = JsonRpcResponse::success(JsonRpcId::Number(42), serde_json::json!({"ok": true}));
        let encoded = serde_json::to_string(&resp).expect("response should serialize");
        let decoded: JsonRpcResponse =
            serde_json::from_str(&encoded).expect("response should deserialize");
        assert_eq!(decoded, resp);
    }

    #[test]
    fn orchestrator_turn_result_roundtrip() {
        let result = OrchestratorTurnResult {
            turn_id: "turn-1".to_string(),
            response: "response".to_string(),
            delegated_runs: vec![DelegatedRun {
                run_id: "run-1".to_string(),
                agent_id: "finance_manager".to_string(),
                state: "completed".to_string(),
                summary: Some("delegation summary".to_string()),
            }],
            tool_invocations: vec![OrchestratorToolInvocation {
                call_id: "call-1".to_string(),
                tool_id: "list_agents".to_string(),
                arguments: serde_json::json!({}),
                status: "success".to_string(),
                output: serde_json::json!({"agents": []}),
            }],
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorTurnResult =
            serde_json::from_str(&encoded).expect("result should deserialize");
        assert_eq!(decoded, result);
    }

    #[test]
    fn orchestrator_runs_list_result_roundtrip() {
        let result = OrchestratorRunsListResult {
            runs: vec![DelegatedRun {
                run_id: "run-1".to_string(),
                agent_id: "planner".to_string(),
                state: "completed".to_string(),
                summary: Some("ok".to_string()),
            }],
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorRunsListResult =
            serde_json::from_str(&encoded).expect("result should deserialize");
        assert_eq!(decoded, result);
    }
}
