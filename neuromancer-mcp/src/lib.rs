mod protocol;
mod server;

pub use protocol::{JsonRpcRequest, JsonRpcResponse, McpToolDefinition};
pub use server::{McpServerHandle, ServerStatus, StdioMcpServer};

use std::collections::HashMap;
use std::sync::Arc;

use neuromancer_core::config::McpServerConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::tool::{AgentContext, ToolCall, ToolOutput, ToolResult, ToolSource, ToolSpec};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Trait for communicating with an MCP server.
#[async_trait::async_trait]
pub trait McpClient: Send + Sync {
    /// List available tools from this server.
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpError>;

    /// Call a tool on this server.
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError>;

    /// Check if the server is healthy.
    async fn health_check(&self) -> Result<(), McpError>;

    /// Shut down the server gracefully.
    async fn shutdown(&self) -> Result<(), McpError>;
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("server not found: {0}")]
    ServerNotFound(String),

    #[error("server not running: {0}")]
    ServerNotRunning(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("server spawn failed: {0}")]
    SpawnFailed(String),

    #[error("timeout waiting for server response")]
    Timeout,
}

impl From<McpError> for NeuromancerError {
    fn from(e: McpError) -> Self {
        NeuromancerError::Tool(ToolError::McpServerDown {
            server_id: e.to_string(),
        })
    }
}

/// Entry in the pool: config + live handle.
struct PoolEntry {
    config: McpServerConfig,
    handle: Option<Arc<dyn McpClient>>,
    cached_tools: Vec<McpToolDefinition>,
}

/// MCP client pool managing connections to configured MCP servers.
pub struct McpClientPool {
    servers: RwLock<HashMap<String, PoolEntry>>,
}

impl McpClientPool {
    /// Create a new pool from configuration.
    pub fn new(configs: HashMap<String, McpServerConfig>) -> Self {
        let servers = configs
            .into_iter()
            .map(|(id, config)| {
                (
                    id,
                    PoolEntry {
                        config,
                        handle: None,
                        cached_tools: Vec::new(),
                    },
                )
            })
            .collect();
        Self {
            servers: RwLock::new(servers),
        }
    }

    /// Start all configured child-process servers.
    pub async fn start_all(&self) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
        let ids: Vec<String> = servers.keys().cloned().collect();
        for id in ids {
            let entry = servers.get(&id).unwrap();
            if entry.handle.is_some() {
                continue;
            }
            let config = entry.config.clone();
            match config.kind {
                neuromancer_core::config::McpServerKind::ChildProcess => {
                    let cmd = config.command.as_ref().ok_or_else(|| {
                        McpError::SpawnFailed(format!(
                            "child_process server '{id}' has no command"
                        ))
                    })?;
                    let server = StdioMcpServer::spawn(
                        &id,
                        cmd,
                        &config.env,
                    )
                    .await?;
                    let client: Arc<dyn McpClient> = Arc::new(server);
                    let tools = client.list_tools().await.unwrap_or_default();
                    info!(server_id = %id, tool_count = tools.len(), "MCP server started");
                    let entry = servers.get_mut(&id).unwrap();
                    entry.handle = Some(client);
                    entry.cached_tools = tools;
                }
                neuromancer_core::config::McpServerKind::Remote => {
                    warn!(server_id = %id, "remote MCP servers not yet implemented");
                }
                neuromancer_core::config::McpServerKind::Builtin => {
                    warn!(server_id = %id, "builtin MCP servers not yet implemented");
                }
            }
        }
        Ok(())
    }

    /// Start a single server by ID.
    pub async fn start_server(&self, server_id: &str) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::ServerNotFound(server_id.to_string()))?;
        if entry.handle.is_some() {
            return Ok(());
        }
        let config = entry.config.clone();
        match config.kind {
            neuromancer_core::config::McpServerKind::ChildProcess => {
                let cmd = config.command.as_ref().ok_or_else(|| {
                    McpError::SpawnFailed(format!(
                        "child_process server '{server_id}' has no command"
                    ))
                })?;
                let server = StdioMcpServer::spawn(server_id, cmd, &config.env).await?;
                let client: Arc<dyn McpClient> = Arc::new(server);
                let tools = client.list_tools().await.unwrap_or_default();
                info!(server_id = %server_id, tool_count = tools.len(), "MCP server started");
                entry.handle = Some(client);
                entry.cached_tools = tools;
            }
            _ => {
                warn!(server_id = %server_id, "server kind not yet implemented");
            }
        }
        Ok(())
    }

    /// Stop a single server.
    pub async fn stop_server(&self, server_id: &str) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::ServerNotFound(server_id.to_string()))?;
        if let Some(handle) = entry.handle.take() {
            handle.shutdown().await?;
            info!(server_id = %server_id, "MCP server stopped");
        }
        entry.cached_tools.clear();
        Ok(())
    }

    /// Shut down all servers.
    pub async fn shutdown_all(&self) {
        let mut servers = self.servers.write().await;
        for (id, entry) in servers.iter_mut() {
            if let Some(handle) = entry.handle.take() {
                if let Err(e) = handle.shutdown().await {
                    warn!(server_id = %id, error = %e, "failed to shut down MCP server");
                }
            }
            entry.cached_tools.clear();
        }
    }

    /// List all tool specs visible to an agent, filtered by their allowed MCP servers.
    pub async fn list_tools_for_agent(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let servers = self.servers.read().await;
        let mut tools = Vec::new();
        for (server_id, entry) in servers.iter() {
            if !ctx.allowed_mcp_servers.contains(server_id) {
                continue;
            }
            for tool_def in &entry.cached_tools {
                tools.push(ToolSpec {
                    id: format!("mcp:{server_id}:{}", tool_def.name),
                    name: tool_def.name.clone(),
                    description: tool_def.description.clone(),
                    parameters_schema: tool_def.input_schema.clone(),
                    source: ToolSource::Mcp {
                        server_id: server_id.clone(),
                    },
                });
            }
        }
        tools
    }

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_id: &str,
        call: &ToolCall,
    ) -> Result<ToolResult, McpError> {
        let servers = self.servers.read().await;
        let entry = servers
            .get(server_id)
            .ok_or_else(|| McpError::ServerNotFound(server_id.to_string()))?;
        let handle = entry
            .handle
            .as_ref()
            .ok_or_else(|| McpError::ServerNotRunning(server_id.to_string()))?;
        match handle.call_tool(&call.tool_id, call.arguments.clone()).await {
            Ok(value) => Ok(ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Success(value),
            }),
            Err(e) => Ok(ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Error(e.to_string()),
            }),
        }
    }

    /// Run health checks on all running servers. Returns IDs of unhealthy servers.
    pub async fn health_check_all(&self) -> Vec<String> {
        let servers = self.servers.read().await;
        let mut unhealthy = Vec::new();
        for (id, entry) in servers.iter() {
            if let Some(ref handle) = entry.handle {
                if handle.health_check().await.is_err() {
                    unhealthy.push(id.clone());
                }
            }
        }
        unhealthy
    }

    /// Get the status of all servers.
    pub async fn server_statuses(&self) -> HashMap<String, ServerStatus> {
        let servers = self.servers.read().await;
        servers
            .iter()
            .map(|(id, entry)| {
                let status = if entry.handle.is_some() {
                    ServerStatus::Running {
                        tool_count: entry.cached_tools.len(),
                    }
                } else {
                    ServerStatus::Stopped
                };
                (id.clone(), status)
            })
            .collect()
    }

    /// Refresh the tool list for a specific server.
    pub async fn refresh_tools(&self, server_id: &str) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::ServerNotFound(server_id.to_string()))?;
        let handle = entry
            .handle
            .as_ref()
            .ok_or_else(|| McpError::ServerNotRunning(server_id.to_string()))?;
        let tools = handle.list_tools().await?;
        entry.cached_tools = tools;
        Ok(())
    }
}
