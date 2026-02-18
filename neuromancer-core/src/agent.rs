use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CapabilityRef;
use crate::secrets::SecretRef;
use crate::task::{Artifact, Checkpoint, TaskId, TaskOutput};
use crate::tool::ToolCall;

pub type AgentId = String;

/// Capabilities granted to an agent — explicit, auditable, revocable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub a2a_peers: Vec<AgentId>,
    /// Agents this agent may request collaboration from (spoke-to-spoke).
    #[serde(default)]
    pub can_request: Vec<AgentId>,
    pub secrets: Vec<SecretRef>,
    pub memory_partitions: Vec<String>,
    pub filesystem_roots: Vec<String>,
    pub network_policy: Option<String>,
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            can_request: Vec::new(),
            secrets: Vec::new(),
            memory_partitions: Vec::new(),
            filesystem_roots: Vec::new(),
            network_policy: None,
        }
    }
}

/// Agent execution mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Run in the same process as the orchestrator.
    Inproc,
    /// Run as a child process.
    Subprocess,
    /// Run in an OCI container.
    Container,
}

/// Configuration for a single sub-agent, loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: AgentId,
    pub mode: AgentMode,
    pub image: Option<String>,
    pub models: AgentModelConfig,
    pub capabilities: AgentCapabilities,
    pub health: AgentHealthConfig,
    pub system_prompt: String,
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelConfig {
    pub planner: Option<String>,
    pub executor: Option<String>,
    pub verifier: Option<String>,
}

impl Default for AgentModelConfig {
    fn default() -> Self {
        Self {
            planner: None,
            executor: None,
            verifier: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealthConfig {
    #[serde(with = "humantime_duration")]
    pub heartbeat_interval: Duration,
    #[serde(with = "humantime_duration")]
    pub watchdog_timeout: Duration,
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

impl Default for AgentHealthConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(30),
            watchdog_timeout: Duration::from_secs(60),
            circuit_breaker: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub failures: u32,
    #[serde(with = "humantime_duration")]
    pub window: Duration,
}

/// Serde helper for human-readable durations like "30s", "10m".
mod humantime_duration {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let secs = duration.as_secs();
        if secs % 60 == 0 && secs >= 60 {
            serializer.serialize_str(&format!("{}m", secs / 60))
        } else {
            serializer.serialize_str(&format!("{}s", secs))
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_duration(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid duration: {s}")))
    }

    fn parse_duration(s: &str) -> Option<Duration> {
        let s = s.trim();
        if let Some(n) = s.strip_suffix('s') {
            n.parse::<u64>().ok().map(Duration::from_secs)
        } else if let Some(n) = s.strip_suffix('m') {
            n.parse::<u64>().ok().map(|m| Duration::from_secs(m * 60))
        } else if let Some(n) = s.strip_suffix('h') {
            n.parse::<u64>().ok().map(|h| Duration::from_secs(h * 3600))
        } else {
            s.parse::<u64>().ok().map(Duration::from_secs)
        }
    }
}

/// Agent execution state machine — tracks each task's lifecycle within an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskExecutionState {
    /// Runtime being initialized: loading skills, connecting MCP, building rig Agent.
    Initializing { task_id: TaskId },

    /// rig Agent is reasoning, preparing the next chat() call.
    Thinking {
        conversation_len: usize,
        iteration: u32,
    },

    /// Agent produced tool calls; executing them.
    Acting {
        pending_calls: Vec<ToolCall>,
        completed_count: usize,
    },

    /// Agent needs external input. Task suspended with checkpoint.
    WaitingForInput {
        question: String,
        checkpoint: Checkpoint,
    },

    /// Agent is waiting for a collaboration request to complete.
    WaitingForCollaboration {
        thread_id: String,
        target_agent: String,
        collaboration_call_id: String,
        checkpoint: Checkpoint,
    },

    /// Agent produced a candidate output — self-assessment or verifier check.
    Evaluating { candidate_output: Artifact },

    /// Task completed successfully.
    Completed { output: TaskOutput },

    /// Task failed.
    Failed {
        error: String,
        partial_output: Option<TaskOutput>,
    },

    /// Task suspended (long-running, crash recovery, or resource pressure).
    Suspended {
        checkpoint: Checkpoint,
        reason: String,
    },
}

/// Report from a sub-agent to the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubAgentReport {
    Progress {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        step: u32,
        description: String,
        artifacts_so_far: Vec<Artifact>,
    },
    InputRequired {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        question: String,
        context: String,
        suggested_options: Vec<String>,
    },
    ToolFailure {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        tool_id: String,
        error: String,
        retry_eligible: bool,
        attempted_count: u32,
    },
    PolicyDenied {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        action: String,
        policy_code: String,
        capability_needed: CapabilityRef,
    },
    Stuck {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        reason: String,
        partial_result: Option<Artifact>,
    },
    Completed {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        artifacts: Vec<Artifact>,
        summary: String,
    },
    Failed {
        task_id: TaskId,
        #[serde(default)]
        thread_id: Option<String>,
        error: String,
        partial_result: Option<Artifact>,
    },
}

/// Orchestrator's response to a sub-agent report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemediationAction {
    Retry {
        max_attempts: u32,
        backoff_ms: u64,
    },
    Clarify {
        additional_context: String,
    },
    GrantTemporary {
        capability: CapabilityRef,
        scope: TaskId,
    },
    Reassign {
        new_agent_id: AgentId,
        reason: String,
    },
    EscalateToUser {
        question: String,
        channel: String,
    },
    Abort {
        reason: String,
    },
}

/// Tracks circuit breaker state for an agent.
#[derive(Debug)]
pub struct CircuitBreakerState {
    pub failure_timestamps: Vec<DateTime<Utc>>,
    pub is_open: bool,
    pub config: CircuitBreakerConfig,
}

impl CircuitBreakerState {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            failure_timestamps: Vec::new(),
            is_open: false,
            config,
        }
    }

    pub fn record_failure(&mut self, now: DateTime<Utc>) {
        let window =
            chrono::Duration::from_std(self.config.window).unwrap_or(chrono::Duration::minutes(10));
        self.failure_timestamps.retain(|t| now - *t < window);
        self.failure_timestamps.push(now);
        if self.failure_timestamps.len() >= self.config.failures as usize {
            self.is_open = true;
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn reset(&mut self) {
        self.failure_timestamps.clear();
        self.is_open = false;
    }
}
