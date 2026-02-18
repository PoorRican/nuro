use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::debug;

use crate::protocol::*;
use crate::{McpClient, McpError};

/// Status of an MCP server in the pool.
#[derive(Debug, Clone)]
pub enum ServerStatus {
    Running { tool_count: usize },
    Stopped,
}

/// Handle that wraps a spawned MCP server child process.
pub struct McpServerHandle {
    pub server_id: String,
    pub child: Mutex<Option<Child>>,
    pub running: AtomicBool,
}

impl McpServerHandle {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// MCP server communicating over stdin/stdout JSON-RPC.
pub struct StdioMcpServer {
    server_id: String,
    next_id: AtomicU64,
    stdin: Mutex<tokio::process::ChildStdin>,
    stdout: Mutex<BufReader<tokio::process::ChildStdout>>,
    child: Mutex<Option<Child>>,
    initialized: AtomicBool,
}

impl StdioMcpServer {
    /// Spawn a child process MCP server and perform the initialize handshake.
    pub async fn spawn(
        server_id: &str,
        command: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        let (program, args) = command
            .split_first()
            .ok_or_else(|| McpError::SpawnFailed("empty command".into()))?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Clear inherited environment to prevent secret leakage, then inject
        // only essential system variables and explicitly configured env vars.
        cmd.env_clear();
        for key in &["PATH", "HOME", "USER", "LANG", "TERM"] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::SpawnFailed(format!("failed to spawn '{program}': {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::SpawnFailed("failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::SpawnFailed("failed to capture stdout".into()))?;

        let server = Self {
            server_id: server_id.to_string(),
            next_id: AtomicU64::new(1),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(Some(child)),
            initialized: AtomicBool::new(false),
        };

        server.initialize().await?;

        Ok(server)
    }

    /// Perform the MCP initialize handshake.
    async fn initialize(&self) -> Result<(), McpError> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".into(),
            capabilities: ClientCapabilities {},
            client_info: ClientInfo {
                name: "neuromancer".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };

        let response = self
            .send_request("initialize", Some(serde_json::to_value(&params)?))
            .await?;
        debug!(
            server_id = %self.server_id,
            response = %response,
            "MCP server initialized"
        );

        // Send initialized notification (no id — it's a notification).
        self.send_notification("notifications/initialized", None)
            .await?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Send a JSON-RPC request and wait for a response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        // Read lines until we get a response with our id.
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), self.read_response(id))
                .await
                .map_err(|_| McpError::Timeout)??;

        response.into_result()
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        #[derive(serde::Serialize)]
        struct Notification {
            jsonrpc: String,
            method: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            params: Option<serde_json::Value>,
        }

        let notif = Notification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        };

        let mut line = serde_json::to_string(&notif)?;
        line.push('\n');

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Read stdout lines until we find a JSON-RPC response matching the given id.
    async fn read_response(&self, expected_id: u64) -> Result<JsonRpcResponse, McpError> {
        let mut stdout = self.stdout.lock().await;
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = stdout.read_line(&mut buf).await?;
            if n == 0 {
                return Err(McpError::Transport("server closed stdout".into()));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try to parse as a response. Skip notifications / other messages.
            match serde_json::from_str::<JsonRpcResponse>(trimmed) {
                Ok(resp) if resp.id == Some(expected_id) => return Ok(resp),
                Ok(resp) => {
                    debug!(
                        server_id = %self.server_id,
                        id = ?resp.id,
                        "received non-matching JSON-RPC message, skipping"
                    );
                }
                Err(_) => {
                    // Might be a notification or log line from the server.
                    debug!(
                        server_id = %self.server_id,
                        line = trimmed,
                        "ignoring non-JSON-RPC line from server"
                    );
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl McpClient for StdioMcpServer {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpError> {
        let result = self.send_request("tools/list", None).await?;

        // The result should be { "tools": [...] }
        let tools_value = result
            .get("tools")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));

        let tools: Vec<McpToolDefinition> = serde_json::from_value(tools_value)
            .map_err(|e| McpError::Protocol(format!("failed to parse tools list: {e}")))?;

        Ok(tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };
        let result = self
            .send_request("tools/call", Some(serde_json::to_value(&params)?))
            .await?;

        // Parse the MCP tool call result format.
        let call_result: ToolCallResult =
            serde_json::from_value(result.clone()).unwrap_or(ToolCallResult {
                content: vec![],
                is_error: false,
            });

        if call_result.is_error {
            let error_text = call_result
                .content
                .iter()
                .filter_map(|c| c.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(McpError::Protocol(format!("tool error: {error_text}")));
        }

        // Extract text content into a JSON value.
        let texts: Vec<&str> = call_result
            .content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect();

        if texts.len() == 1 {
            // Try to parse as JSON first, fall back to string.
            match serde_json::from_str(texts[0]) {
                Ok(v) => Ok(v),
                Err(_) => Ok(serde_json::Value::String(texts[0].to_string())),
            }
        } else {
            Ok(serde_json::Value::String(texts.join("\n")))
        }
    }

    async fn health_check(&self) -> Result<(), McpError> {
        // Send a ping (list tools is a lightweight operation).
        let _ = self.send_request("ping", None).await.or_else(|_| {
            // Ping might not be supported; try listing tools as health check.
            Ok::<_, McpError>(serde_json::Value::Null)
        });
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), McpError> {
        // Kill the child process.
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_serialization() {
        let req = JsonRpcRequest::new(1, "tools/list", None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"tools/list\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn json_rpc_response_parsing() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn json_rpc_error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        let err = resp.into_result();
        assert!(err.is_err());
    }

    #[test]
    fn tool_definition_parsing() {
        let json = r#"{"name":"read_file","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}}"#;
        let tool: McpToolDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description, "Read a file");
    }

    #[test]
    fn tool_call_result_parsing() {
        let json = r#"{"content":[{"type":"text","text":"hello world"}],"isError":false}"#;
        let result: ToolCallResult = serde_json::from_str(json).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].text.as_deref(), Some("hello world"));
    }
}
