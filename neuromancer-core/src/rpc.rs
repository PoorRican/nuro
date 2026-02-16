use std::collections::BTreeMap;

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
    pub thread_id: Option<String>,
    pub initial_instruction: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegatedTask {
    pub run_id: String,
    pub agent_id: String,
    pub thread_id: String,
    pub state: String,
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
    pub delegated_tasks: Vec<DelegatedTask>,
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
pub struct ThreadSummary {
    pub thread_id: String,
    pub kind: String,
    pub agent_id: Option<String>,
    pub latest_run_id: Option<String>,
    pub state: String,
    pub updated_at: String,
    pub resurrected: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorThreadsListResult {
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorThreadGetParams {
    pub thread_id: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub event_id: String,
    pub thread_id: String,
    pub thread_kind: String,
    pub seq: u64,
    pub ts: String,
    pub event_type: String,
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
    pub payload: serde_json::Value,
    pub redaction_applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorThreadGetResult {
    pub thread_id: String,
    pub events: Vec<ThreadEvent>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorThreadResurrectParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorThreadResurrectResult {
    pub thread: ThreadSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorSubagentTurnParams {
    pub thread_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorSubagentTurnResult {
    pub thread_id: String,
    pub run: DelegatedRun,
    pub response: String,
    pub tool_invocations: Vec<OrchestratorToolInvocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorOutputsPullParams {
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorOutputItem {
    pub output_id: String,
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    pub state: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub no_reply: bool,
    pub content: Option<String>,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorOutputsPullResult {
    pub outputs: Vec<OrchestratorOutputItem>,
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorEventsQueryParams {
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub tool_id: Option<String>,
    pub event_type: Option<String>,
    pub error_contains: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorEventsQueryResult {
    pub events: Vec<ThreadEvent>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorRunDiagnoseParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorRunDiagnoseResult {
    pub run: DelegatedRun,
    pub thread: Option<ThreadSummary>,
    pub events: Vec<ThreadEvent>,
    pub inferred_failure_cause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorStatsGetResult {
    pub threads_total: usize,
    pub events_total: usize,
    pub runs_total: usize,
    pub failed_runs: usize,
    pub event_counts: BTreeMap<String, usize>,
    pub tool_counts: BTreeMap<String, usize>,
    pub agent_counts: BTreeMap<String, usize>,
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
            delegated_tasks: vec![DelegatedTask {
                run_id: "run-1".to_string(),
                agent_id: "finance_manager".to_string(),
                thread_id: "thread-1".to_string(),
                state: "queued".to_string(),
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
    fn orchestrator_outputs_pull_result_roundtrip() {
        let result = OrchestratorOutputsPullResult {
            outputs: vec![OrchestratorOutputItem {
                output_id: "out-1".to_string(),
                run_id: "run-1".to_string(),
                thread_id: "thread-1".to_string(),
                agent_id: "planner".to_string(),
                state: "completed".to_string(),
                summary: Some("finished".to_string()),
                error: None,
                no_reply: false,
                content: Some("raw output text".to_string()),
                published_at: "2026-02-16T00:00:00Z".to_string(),
            }],
            remaining: 0,
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorOutputsPullResult =
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
                thread_id: Some("thread-1".to_string()),
                initial_instruction: Some("do thing".to_string()),
                error: None,
            }],
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorRunsListResult =
            serde_json::from_str(&encoded).expect("result should deserialize");
        assert_eq!(decoded, result);
    }

    #[test]
    fn thread_event_roundtrip() {
        let event = ThreadEvent {
            event_id: "e1".to_string(),
            thread_id: "thread-1".to_string(),
            thread_kind: "subagent".to_string(),
            seq: 42,
            ts: "2026-02-14T12:00:00Z".to_string(),
            event_type: "message_assistant".to_string(),
            agent_id: Some("finance-manager".to_string()),
            run_id: Some("run-1".to_string()),
            payload: serde_json::json!({"content":"hello"}),
            redaction_applied: false,
            turn_id: Some("turn-1".to_string()),
            parent_event_id: None,
            call_id: None,
            attempt: None,
            duration_ms: Some(25),
            meta: Some(serde_json::json!({"note":"ok"})),
        };

        let encoded = serde_json::to_string(&event).expect("event should serialize");
        let decoded: ThreadEvent =
            serde_json::from_str(&encoded).expect("event should deserialize");
        assert_eq!(decoded, event);
    }

    #[test]
    fn orchestrator_context_get_result_roundtrip() {
        let result = OrchestratorContextGetResult {
            messages: vec![
                OrchestratorThreadMessage::Text {
                    role: "assistant".to_string(),
                    content: "hello".to_string(),
                },
                OrchestratorThreadMessage::ToolInvocation {
                    call_id: "call-1".to_string(),
                    tool_id: "delegate_to_agent".to_string(),
                    arguments: serde_json::json!({"agent_id": "planner"}),
                    status: "success".to_string(),
                    output: serde_json::json!({"run_id": "run-1"}),
                },
            ],
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorContextGetResult =
            serde_json::from_str(&encoded).expect("result should deserialize");
        assert_eq!(decoded, result);
    }

    #[test]
    fn orchestrator_stats_get_result_roundtrip() {
        let result = OrchestratorStatsGetResult {
            threads_total: 2,
            events_total: 12,
            runs_total: 3,
            failed_runs: 1,
            event_counts: BTreeMap::from([
                ("message_user".to_string(), 3),
                ("tool_result".to_string(), 4),
            ]),
            tool_counts: BTreeMap::from([("delegate_to_agent".to_string(), 2)]),
            agent_counts: BTreeMap::from([("planner".to_string(), 2)]),
        };

        let encoded = serde_json::to_string(&result).expect("result should serialize");
        let decoded: OrchestratorStatsGetResult =
            serde_json::from_str(&encoded).expect("result should deserialize");
        assert_eq!(decoded, result);
    }
}
