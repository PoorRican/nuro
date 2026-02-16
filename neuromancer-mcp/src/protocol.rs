use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request for MCP protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 response from MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn into_result(self) -> Result<serde_json::Value, crate::McpError> {
        if let Some(err) = self.error {
            Err(crate::McpError::Protocol(format!(
                "JSON-RPC error {}: {}",
                err.code, err.message
            )))
        } else {
            Ok(self.result.unwrap_or(serde_json::Value::Null))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// MCP tool definition as returned by tools/list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_schema")]
    pub input_schema: serde_json::Value,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

/// MCP initialize request params.
#[derive(Debug, Serialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Serialize)]
pub struct ClientCapabilities {}

#[derive(Debug, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// MCP tools/call request params.
#[derive(Debug, Serialize)]
pub struct ToolCallParams {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// MCP tools/call result content.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallResult {
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
}
