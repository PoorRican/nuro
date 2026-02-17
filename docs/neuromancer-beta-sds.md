# Neuromancer Beta System Design Specification

## 0. Status, scope, and lineage

**Status:** Beta v1 runtime specification — compiled from the v0.0.2-alpha SDS and two rounds of architectural deliberation.
**Scope:** A Rust daemon ("Neuromancer") providing a **System0 LLM orchestrator** supervising isolated **rig-powered sub-agents** with brokered secrets, declarative capabilities, and end-to-end observability.

This document is the *implementation target*. It preserves the strong architectural foundations of the alpha SDS while cutting scope to what ships and is useful within weeks.

**What changed from alpha SDS:**

* Sub-agent collaboration formalized as two distinct paths: programmatic spoke-to-spoke (`request_agent_assistance`) and LLM-mediated orchestration (via `Stuck` reports)
* System0 persistent session explicitly specified (conversation log backed by SQLite, truncation/summarization lifecycle)
* Skills scoped to: loader + permission validation + degraded mode + scrubbed-environment script runner
* A2A deferred entirely (protocol is immature; internal collaboration doesn't need HTTP)
* Container isolation deferred (all agents in-process for beta)
* Self-improvement proposal lifecycle deferred (externalize to Claude Code skill for authoring/configuration)
* Admin API requires bearer token auth even in beta
* MCP child process environment scrubbing made explicit
* Outbound secret scanner (Aho-Corasick) specified at four chokepoints for exfiltration prevention
* OTP/2FA handling formalized: autonomous TOTP generation vs human-in-the-loop escalation, per-secret policy
* Secret kinds expanded: Credential, TotpSeed (with autonomous/escalate policy), CertificateOrKey
* Memory partitioning simplified to per-agent + shared workspace (no org/global hierarchy)
* Extraction agent formalized as the durable knowledge distillation mechanism

**Explicitly deferred to post-beta:**

* A2A protocol integration (external agent-to-agent)
* Container-based agent isolation
* Self-improvement proposal/canary/rollback lifecycle (7-stage)
* Skill linting, provenance, and marketplace machinery
* Multi-workspace/org memory hierarchy
* Verifier gate and heuristic gates in the policy engine
* Progressive skill loading optimization
* Skill execution mode: isolated/container

---

## 1. Motivation and problem statement

Neuromancer is designed around failures repeatedly seen in the wild:

* **Secrets leaking into model context and logs**: resolved provider API keys serialized into the LLM prompt on every turn, persisting in provider logs. ([GitHub][1])
* **"Leaky skills" as a systemic pattern**: security research found a non-trivial fraction of marketplace skills that instruct the agent to mishandle secrets/PII. ([Snyk][2])
* **Long-running instability and process leakage**: orphaned child processes, multi-GB memory pressure, degraded processing latency. ([GitHub][3])
* **Supply chain reality**: the missing ingredients are provenance, mediated execution, and specific + revocable permissions tied to identity. ([1Password][4])

**Neuromancer** is defined by: **least privilege**, **isolation**, **brokered secrets**, and **observable operations**, while staying compatible with the ecosystems forming around MCP and skills.

---

## 2. Goals and non-goals

### 2.1 Goals

1. **Daemon-first architecture**
   A resilient, long-running service with explicit lifecycle management and resource controls.

2. **System0 orchestration with persistent session**
   System0 is a first-class LLM runtime that maintains conversational continuity across turns. It handles `orchestrator.turn` ingress directly, maintains a persistent conversation log, and delegates complex execution to sub-agents via controlled tooling.

3. **No new plugin protocol**
   * Tools/services → **MCP**
   * Instruction packaging → **Skills**
   The "tool surface" is unified internally, but external "plugins" are expected to be MCP servers.

4. **Security by construction**
   * Secrets never go into LLM context by default
   * Capabilities are explicit and auditable
   * Sub-agents run isolated with a clear request path for out-of-bounds capabilities
   * MCP child processes spawned with scrubbed environments

5. **Full observability**
   Structured traces/spans for: trigger → task → LLM calls → tool calls → secret accesses → memory reads/writes. Export via OpenTelemetry OTLP.

6. **Sub-agent collaboration**
   Two-path model: programmatic spoke-to-spoke for known collaborations, LLM-mediated orchestration for ambiguous situations.

7. **Admin API for runtime introspection**
   Localhost admin surface with JSON-RPC methods, bearer token auth.

### 2.2 Non-goals

* A public skill marketplace / package manager.
* Full multi-tenant enterprise RBAC UI.
* A polished web UI; a TUI will be provided.
* A2A protocol integration (deferred).
* Container-based agent isolation (deferred).
* Self-improvement proposal lifecycle in-runtime (use Claude Code externally).

---

## 3. High-level architecture

Neuromancer is split into **control plane** and **data plane**. The control plane is System0, implemented as an LLM agent runtime with policy-gated control-plane tooling.

### 3.1 Control plane: `neuromancerd` + System0 orchestrator

The daemon process hosts **System0**, which mediates user/admin turns and delegates execution to sub-agents. The orchestrator:

* Loads **central TOML config**
* Runs **Trigger Manager** (Discord, cron)
* Maintains **Secrets Broker**, **Memory Store**, **MCP Client Pool**
* Accepts public ingress from `orchestrator.turn` and processes one queued turn at a time
* Maintains a **persistent conversation log** (System0's ongoing session, backed by SQLite)
* Supervises sub-agent runtimes (in-process for beta)
* Manages delegated-run tracking and audit trail
* Facilitates cross-agent communication via programmatic routing and remediation
* Provides an **Admin API** on localhost (bearer token auth)

System0 is a dynamic router and control-plane LLM agent. It facilitates user↔sub-agent communication and administrative actions via tool calls. Outbound actions are constrained by an allowlisted tool broker and trigger/capability policy.

**Key architectural invariants:**

* Public ingress is `orchestrator.turn` and the chat UI.
* System0 is a rig-backed **dynamic router** agent (LLM-powered, tool-first) that mediates turns, delegates work, and maintains a single persistent session through `neuromancer-agent`.
* All user inputs (CLI and external channels such as Discord) are normalized into System0 turn processing.
* Only the CLI path currently emits `TriggerType::Admin`; other ingress paths are non-admin by default.
* Mutation privilege boundary is `TriggerType::Admin` only.

```rust
struct Orchestrator {
    config: Arc<NeuromancerConfig>,
    input_message_queue: InputMessageQueue,
    delegated_run_registry: DelegatedRunRegistry,
    system0_runtime: AgentRuntime,
    system0_session: PersistentSession,  // SQLite-backed conversation log
    memory_store: Arc<dyn MemoryStore>,
    secrets_broker: Arc<dyn SecretsBroker>,
    mcp_pool: McpClientPool,
    policy_engine: Arc<dyn PolicyEngine>,
}

impl Orchestrator {
    /// Enqueue one public inbound turn from CLI/admin RPC.
    async fn orchestrator_turn(&self, message: String) -> Result<OrchestratorTurnResult>;

    /// Execute one System0 turn against the persistent session.
    async fn process_turn(&self, turn: InputTurn) -> Result<OrchestratorTurnResult>;

    /// Enqueue a task directly (bypasses System0 for spoke-to-spoke).
    async fn enqueue_direct(&self, task: Task) -> Result<TaskId>;

    /// Block until a task reaches a terminal state.
    async fn await_task_result(&self, task_id: TaskId, timeout: Duration) -> Result<TaskOutput>;

    /// Read-only delegated run introspection.
    async fn list_runs(&self) -> Result<Vec<DelegatedRun>>;
    async fn get_run(&self, run_id: String) -> Result<DelegatedRun>;
}
```

### 3.2 Data plane: sub-agent runtimes (isolated domain execution)

Each sub-agent runtime wraps a **rig Agent** configured with a strict **capability set**:

* Allowed skills
* Allowed MCP servers/tools
* Allowed secrets (by handle)
* Allowed memory partitions
* Allowed filesystem roots
* Allowed outbound network policy
* Allowed peer agents for collaboration (`can_request`)

Both System0 and sub-agents can call LLMs. System0 handles dynamic routing and control-plane mediation; sub-agents perform specialized domain execution.

### 3.3 Architecture diagram

```
                        +-------------------------------+
Discord / Cron -------->|     neuromancerd (control)     |<-------- Admin API
  Triggers              |  Orchestrator (System0 LLM)    |         (localhost)
                        |  - turn ingress queue          |
                        |  - persistent session (SQLite) |
                        |  - delegated run tracking      |
                        |  - secrets broker              |
                        |  - memory store                |
                        |  - policy engine               |
                        |  - OTEL exporter               |
                        +-------+----------+--------+----+
                                |          |        |
                     dispatch   |          |        |   dispatch
                    +-----------+    +-----+--+     +----------+
                    v                v        |                v
          +---------+------+ +------+--------++ +-------------+----+
          | Agent: planner | | Agent: browser | | Agent: files     |
          | (rig Agent)    | | (rig Agent)    | | (rig Agent)      |
          | skills: [plan] | | skills: [web]  | | skills: [fs]     |
          | can_request:   | | can_request:   | | can_request: []  |
          |   [browser]    | |   []           | |                  |
          +-------+--------+ +-------+--------+ +--------+--------+
                  |                   |                    |
                  | MCP (via rig)     | MCP (via rig)      | MCP (via rig)
                  v                   v                    v
           [MCP servers...]   [Playwright MCP]    [FS MCP / built-in]
```

### 3.4 Orchestrator module structure

* `orchestrator/runtime.rs` — orchestrator turn loop + RPC-facing runtime methods
* `orchestrator/state.rs` — System0 broker/state containers, persistent session management
* `orchestrator/actions/`
  * `runtime_actions.rs` — `delegate_to_agent`, `list_agents`, `read_config`
  * `dispatch.rs` — tool class routing
* `orchestrator/security/` — trigger gate, audit records, execution guard, redaction
* `orchestrator/collaboration/` — `request_agent_assistance` routing and task awaiting
* `orchestrator/tracing/` — thread journal, event query filtering, conversation projection, JSONL I/O
* `orchestrator/skills/` — skill broker, aliasing, path policy

---

## 4. Protocol choices

### 4.1 MCP for tools

Neuromancer integrates tools via **MCP**, using the **official Rust SDK** (`rmcp`). ([GitHub][5])

Tools are surfaced to rig agents via rig's `.mcp_tool()` integration. This is the primary path for browser automation, file tooling, external connectors, and sidecar services.

### 4.2 Skills as instruction packaging

Skills are **declarative bundles**: metadata, instructions, resources, and explicit permission requirements. They are not a new protocol — they are the "instruction + packaging" layer that sits on top of MCP tools. See §8.

---

## 5. Beta functional requirements

| Requirement | Implementation |
|---|---|
| Minimum, extensible core for running an agent | `neuromancer-core` library crate with core traits. `neuromancer-agent` crate with rig-based runtime. |
| Detailed logging via OpenTelemetry | `tracing` + OTLP exporter; spans for every step. |
| Sub-agents can run skills, use MCP | ToolBroker with 2 backends: SkillScriptRunner, McpClientPool (via rig `.mcp_tool()`). |
| Sub-agents can collaborate programmatically | `request_agent_assistance` tool with programmatic policy checks. |
| Centralized configuration (TOML) | Single TOML file defines models, agents, triggers, servers, secrets, memory. |
| Trigger system to orchestrator | Trigger Manager emits events into System0 input queue. |
| Triggers: Discord and cron | Discord via `twilight`; cron via `tokio-cron-scheduler`. |
| Secure secrets store with access controls | Secrets Broker with encrypted-at-rest store, ACLs, handle-based injection, outbound exfiltration scanner. |
| OTP/2FA handling | Autonomous TOTP generation or human escalation, per-secret policy. |
| Per-agent memory with shared workspace | Memory partitions per agent + shared workspace. |
| System0 persistent session | SQLite-backed conversation log with truncation/summarization. |
| Extraction agent for knowledge distillation | Dedicated agent that converts task summaries into durable facts. |
| Admin API for runtime introspection | Localhost axum surface with JSON-RPC + bearer token auth. |

---

## 6. Core runtime design

### 6.1 Core interfaces (traits)

These traits define the modularity and testability surface. They live in `neuromancer-core`.

```rust
// Neuromancer does not define its own LLM abstraction. Use rig's
// CompletionModel and Agent directly. See §7 for rig integration.

/// Policy-enforcing tool broker. Wraps rig tool definitions with
/// capability checks before execution.
trait ToolBroker: Send + Sync {
    /// List tools visible to this agent context, filtered by policy.
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec>;

    /// Execute a tool call after policy checks.
    async fn call_tool(&self, ctx: &AgentContext, call: ToolCall) -> Result<ToolResult>;
}

/// Persistent, partitioned memory store. See §13.
trait MemoryStore: Send + Sync {
    async fn put(&self, ctx: &AgentContext, item: MemoryItem) -> Result<MemoryId>;
    async fn query(&self, ctx: &AgentContext, q: MemoryQuery) -> Result<MemoryPage>;
    async fn get(&self, ctx: &AgentContext, id: MemoryId) -> Result<Option<MemoryItem>>;
    async fn delete(&self, ctx: &AgentContext, id: MemoryId) -> Result<()>;
    async fn gc(&self) -> Result<u64>;
}

/// Handle-based secret resolution. Secrets never enter LLM context.
trait SecretsBroker: Send + Sync {
    async fn resolve_handle_for_tool(
        &self,
        ctx: &AgentContext,
        secret_ref: SecretRef,
        usage: SecretUsage,
    ) -> Result<ResolvedSecret>;
}

/// Policy gates evaluated before/after tool and LLM calls.
/// Beta implements static gates only.
trait PolicyEngine: Send + Sync {
    async fn pre_tool_call(&self, ctx: &AgentContext, call: &ToolCall) -> PolicyDecision;
    async fn pre_llm_call(&self, ctx: &AgentContext, messages: &[ChatMessage]) -> PolicyDecision;
    async fn post_tool_call(&self, ctx: &AgentContext, result: &ToolResult) -> PolicyDecision;
}
```

### 6.2 Orchestrator loop

The orchestrator is the main event loop of `neuromancerd`. It runs as a System0 `neuromancer-agent` instance with a persistent session.

```
loop {
    select! {
        turn = input_message_queue.recv() => {
            // Load recent conversation history from persistent session
            // Execute System0 turn
            // Flush updated conversation to persistent session
            system0_agent.execute_turn(system0_session, turn.message).await?;
        }
        delegated = delegated_run_updates.recv() => {
            delegated_registry.update(delegated);
        }
        _ = shutdown_signal() => break,
    }
}
```

Public ingress is message-turn based:

* `orchestrator.turn` enqueues one input message.
* Runtime loads System0's persistent conversation history, executes one turn, flushes updated history.
* External channel inputs and CLI inputs are both routed through System0.
* Only CLI-originated turns carry `TriggerType::Admin`.
* Delegated runs are tracked internally and exposed via `orchestrator.runs.list` / `orchestrator.runs.get`.

### 6.3 Agent execution state machine

Each sub-agent task execution follows a state machine.

```rust
enum TaskExecutionState {
    /// Agent runtime being initialized: loading skills, connecting MCP, building rig Agent.
    Initializing { task: Task },

    /// rig Agent is reasoning: preparing the next chat() call.
    /// `iteration` counts Thinking→Acting cycles (capped by max_iterations).
    Thinking {
        conversation: EphemeralSession,
        iteration: u32,
    },

    /// Agent has produced tool calls; executing them.
    Acting {
        conversation: EphemeralSession,
        pending_calls: Vec<ToolCall>,
        completed_calls: Vec<ToolResult>,
    },

    /// Agent needs external input (human clarification or agent assistance).
    /// Task is suspended with a checkpoint so it can resume when input arrives.
    WaitingForInput {
        conversation: EphemeralSession,
        question: String,
        checkpoint: Checkpoint,
    },

    /// Agent has produced a candidate output.
    Evaluating {
        conversation: EphemeralSession,
        candidate_output: Artifact,
    },

    /// Task completed successfully.
    Completed { output: TaskOutput },

    /// Task failed.
    Failed {
        error: AgentError,
        partial_output: Option<TaskOutput>,
    },

    /// Task suspended (long-running, crash recovery, or resource pressure).
    Suspended {
        checkpoint: Checkpoint,
        reason: String,
    },
}
```

**State transitions:**

```
Initializing ──→ Thinking ──→ Acting ──→ Thinking  (loop)
                    │            │
                    │            ├──→ WaitingForInput ──→ Thinking (on input)
                    │            │
                    ├──→ Evaluating ──→ Completed
                    │                   ──→ Thinking (refinement)
                    │
                    ├──→ Failed
                    ├──→ Suspended
```

**Key behaviors:**

* **Thinking → Acting → Thinking** loop continues until the agent produces a final output or hits `max_iterations`.
* Each cycle corresponds to one `agent.chat()` call in rig. The LLM response may contain tool calls (→ Acting) or a final answer (→ Evaluating or Completed).
* **Checkpointing** occurs at every state transition. Checkpoint includes serialized `EphemeralSession` and current state, persisted to SQLite.
* **WaitingForInput** is entered when the agent explicitly requests human clarification OR when `request_agent_assistance` is awaiting a collaborating agent's result.
* **Evaluating** enables self-assessment: the agent (or a separate verifier) reviews the candidate output. May loop back to Thinking for refinement.

### 6.4 Conversation contexts: persistent vs ephemeral

Neuromancer uses two distinct session types sharing a common message structure but with different lifetime semantics.

```rust
enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

struct ChatMessage {
    role: MessageRole,
    content: MessageContent,
    timestamp: DateTime<Utc>,
    token_estimate: u32,
    metadata: MessageMetadata,
}

enum MessageContent {
    Text(String),
    ToolCall(Vec<ToolCall>),
    ToolResult(ToolResult),
    Mixed(Vec<ContentPart>),
}

struct MessageMetadata {
    source: Option<String>,       // "discord", "cron", "memory", "tool:<tool_id>"
    parent_message_id: Option<u64>,
    redacted: bool,
}
```

#### 6.4.1 PersistentSession (System0)

System0 maintains a single ongoing conversation across all turns. This is backed by SQLite.

```rust
struct PersistentSession {
    session_id: SessionId,
    /// SQLite table: system0_conversation_log
    /// Columns: id, role, content, timestamp, token_estimate, metadata_json
    db: SqlitePool,
    /// How many tokens of history to load into context on each turn
    context_window_budget: u32,
    /// How to handle overflow
    truncation_strategy: TruncationStrategy,
}

impl PersistentSession {
    /// Load the most recent messages that fit within the context window budget.
    /// Always includes the system prompt.
    /// Injects relevant MemoryStore items as system messages after the prompt.
    async fn load_context(&self, memory_store: &dyn MemoryStore) -> Vec<ChatMessage>;

    /// Append new messages from this turn to the persistent log.
    async fn flush(&self, new_messages: &[ChatMessage]) -> Result<()>;

    /// When context window fills: summarize older messages into a MemoryStore
    /// item, then advance the active window start pointer.
    async fn maybe_compact(
        &self,
        memory_store: &dyn MemoryStore,
        summarizer_model: &dyn CompletionModel,
    ) -> Result<()>;
}
```

**Compaction lifecycle:**

1. On each turn, after flushing new messages, check if `token_used > threshold_pct * context_window_budget`.
2. If threshold exceeded: summarize the oldest N messages (outside the recent window) into a `kind: summary` memory item written to `workspace:default`.
3. Mark the summarized messages as compacted in the log (they remain on disk for audit but are no longer loaded into context).
4. Next turn loads: system prompt + injected memory items (including the summary) + recent un-compacted messages.

This replaces OpenClaw's "silent NO_REPLY pre-compaction flush" with a first-class, non-hacky compaction path.

#### 6.4.2 EphemeralSession (sub-agents)

Sub-agents receive a fresh context per task. It lives for the duration of one task execution.

```rust
struct EphemeralSession {
    messages: Vec<ChatMessage>,
    token_budget: u32,
    token_used: u32,
    truncation_strategy: TruncationStrategy,
}

enum TruncationStrategy {
    /// Keep the last N messages, always preserving the system prompt.
    SlidingWindow { keep_last: usize },
    /// When token_used exceeds threshold_pct of token_budget, summarize
    /// older messages using the specified model.
    Summarize { summarizer_model: String, threshold_pct: f32 },
    /// Hard truncation: drop oldest messages when budget exceeded.
    Strict,
}
```

**Lifetime comparison:**

| Aspect | PersistentSession | EphemeralSession |
|---|---|---|
| Owner | System0 | Sub-agents |
| Lifetime | Across all turns (daemon lifetime) | Single task execution |
| Backing store | SQLite conversation log | In-memory Vec |
| Compaction | Summarize to MemoryStore, advance window | Per TruncationStrategy |
| Crash recovery | Reload from SQLite | Reload from checkpoint |

**Sub-agent context initialization:**

When the orchestrator dispatches a task to a sub-agent, the sub-agent receives a fresh `EphemeralSession` containing:

1. The sub-agent's own system prompt (with active skill instructions injected)
2. Relevant memory items for the sub-agent's authorized partitions
3. A context slice from the parent task (if any): the task instruction + summary of prior context, NOT the full parent conversation

**Message ordering in the context window:**

1. System prompt (agent identity, personality, constraints, active skill instructions)
2. Memory context (relevant items from MemoryStore, injected as system messages)
3. Conversation history (accumulated ChatMessages from prior iterations)
4. Current user message / task instruction

### 6.5 Sub-agent collaboration: two-path model

When a sub-agent needs capabilities it doesn't have, there are two distinct paths depending on whether the collaboration target is known or unknown.

#### Path 1: Programmatic spoke-to-spoke (`request_agent_assistance`)

For **known collaborations** where the calling agent has `capabilities.can_request` configured for the target agent. No LLM involved in routing — this is a programmatic policy check and task dispatch.

```rust
/// Available to sub-agents whose config includes can_request.
/// Registered as a rig Tool on the agent at construction time.
struct RequestAgentAssistance {
    orchestrator: Arc<Orchestrator>,
}

impl Tool for RequestAgentAssistance {
    type Input = AssistanceRequest;
    type Output = AssistanceResult;

    async fn call(&self, ctx: &AgentContext, input: AssistanceRequest) -> Result<ToolResult> {
        // 1. Programmatic policy check (no LLM)
        let caller_caps = ctx.agent_config().capabilities;
        if !caller_caps.can_request.contains(&input.target_agent) {
            return Err(ToolError::CapabilityDenied {
                agent_id: ctx.agent_id(),
                capability: format!("can_request:{}", input.target_agent),
            });
        }

        // 2. Create task for target agent
        let task = Task {
            instruction: input.instruction,
            assigned_agent: input.target_agent.clone(),
            parent_id: Some(ctx.current_task_id()),
            priority: TaskPriority::Normal,
            ..Default::default()
        };

        // 3. Enqueue directly (bypasses System0 turn processing)
        let task_id = self.orchestrator.enqueue_direct(task).await?;

        // 4. Block until result (with timeout)
        //    Calling agent transitions to WaitingForInput during this wait
        let result = self.orchestrator
            .await_task_result(task_id, input.timeout.unwrap_or(Duration::from_secs(300)))
            .await?;

        // 5. Return as ToolResult to calling agent
        Ok(ToolResult::from(result))
    }
}

struct AssistanceRequest {
    target_agent: AgentId,
    instruction: String,
    /// Optional context to pass to the target agent
    context: Option<String>,
    /// Timeout for the assistance request. Default: 300s.
    timeout: Option<Duration>,
}
```

**What this enables:** The planner agent can call `request_agent_assistance(target: "browser", instruction: "scrape this URL and return the content")` and receive the result inline in its own conversation context. The browser agent runs its full task lifecycle, and the result flows back as a `ToolResult`.

**What System0 sees:** The `DelegatedRunRegistry` tracks the spawned task. OTEL spans log the full exchange. If you query `orchestrator.runs.list`, the inter-agent dispatch shows up. But System0 was not in the hot path.

**Error handling:** If the target agent fails, the calling agent receives a `ToolError` and can decide to retry, work around it, or report `Stuck`. This preserves the layered escalation model.

#### Path 2: LLM-mediated orchestration (via `Stuck` / `PolicyDenied`)

For **unknown capability gaps** where the sub-agent doesn't know what it needs or the situation is ambiguous.

```rust
enum SubAgentReport {
    /// Periodic progress update during long-running tasks.
    Progress {
        task_id: TaskId,
        step: u32,
        description: String,
        artifacts_so_far: Vec<Artifact>,
    },

    /// Agent needs external input to continue.
    InputRequired {
        task_id: TaskId,
        question: String,
        context: String,
        suggested_options: Vec<String>,
    },

    /// A tool call failed.
    ToolFailure {
        task_id: TaskId,
        tool_id: String,
        error: ToolError,
        retry_eligible: bool,
        attempted_count: u32,
    },

    /// Policy engine denied a requested action.
    PolicyDenied {
        task_id: TaskId,
        action: String,
        policy_code: String,
        capability_needed: CapabilityRef,
    },

    /// Agent is stuck: exceeded iterations, circular reasoning, or no progress.
    Stuck {
        task_id: TaskId,
        reason: String,
        partial_result: Option<Artifact>,
    },

    /// Task completed successfully.
    Completed {
        task_id: TaskId,
        artifacts: Vec<Artifact>,
        summary: String,
    },

    /// Task failed unrecoverably.
    Failed {
        task_id: TaskId,
        error: AgentError,
        partial_result: Option<Artifact>,
    },
}
```

When System0 receives a `Stuck` or `PolicyDenied` report, it uses LLM reasoning to determine the appropriate remediation:

```rust
enum RemediationAction {
    /// Retry the failed operation with backoff.
    Retry { max_attempts: u32, backoff: Duration },

    /// Inject additional context and re-dispatch to the same agent.
    Clarify { additional_context: String },

    /// Grant a temporary capability scoped to this task only.
    GrantTemporary { capability: CapabilityRef, scope: TaskId },

    /// Reassign the task to a different agent.
    Reassign { new_agent_id: AgentId, reason: String },

    /// Escalate to the user via the trigger channel.
    EscalateToUser { question: String, channel: TriggerChannel },

    /// Abort the task.
    Abort { reason: String },
}
```

**Escalation hierarchy:** programmatic spoke-to-spoke → agent self-handling on error → LLM-mediated orchestration via System0 → human escalation.

### 6.6 Health monitoring

* **Heartbeat:** Sub-agents send `Progress` reports at configurable intervals (default: 30s). If no report arrives within `2 * heartbeat_interval`, the orchestrator marks the agent as unresponsive.
* **Watchdog:** Unresponsive agents are sent a cancellation signal. If they don't respond within `watchdog_timeout` (default: 60s), the orchestrator kills the runtime and marks the task as `Failed`.
* **Circuit breaker:** If an agent produces N failures within M minutes (configurable), the orchestrator stops dispatching new tasks to it and emits an alert. Default: 5 failures in 10 minutes.

```toml
[agents.browser.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"
circuit_breaker = { failures = 5, window = "10m" }
```

### 6.7 Task queue and lifecycle

```rust
struct Task {
    id: TaskId,
    parent_id: Option<TaskId>,
    trigger_source: TriggerSource,
    instruction: String,
    assigned_agent: AgentId,
    state: TaskState,
    priority: TaskPriority,
    deadline: Option<DateTime<Utc>>,
    checkpoints: Vec<Checkpoint>,
    idempotency_key: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

enum TaskState {
    Queued,
    Dispatched,
    Running { execution_state: TaskExecutionState },
    Completed { output: TaskOutput },
    Failed { error: AgentError },
    Cancelled { reason: String },
}

enum TaskPriority { Low, Normal, High, Critical }

struct TaskOutput {
    artifacts: Vec<Artifact>,
    summary: String,
    token_usage: TokenUsage,
    duration: Duration,
}
```

**Queue implementation:**

* In-process: `tokio::sync::mpsc` channel for low-latency dispatch.
* Persistence: All tasks written to SQLite on state transitions for crash recovery. On startup, reload tasks in `Queued`/`Dispatched` state.
* Priority ordering: Tasks dequeued by priority (Critical > High > Normal > Low), then by creation time.

**Idempotency:** Tasks with an `idempotency_key` are deduplicated. Before enqueuing, check if a task with the same key exists in a non-terminal state. If so, drop the new task and return the existing task's ID.

### 6.8 Error taxonomy

```rust
enum NeuromancerError {
    Agent(AgentError),
    Llm(LlmError),
    Tool(ToolError),
    Policy(PolicyError),
    Infra(InfraError),
}

enum AgentError {
    MaxIterationsExceeded { task_id: TaskId, iterations: u32 },
    InvalidToolCall { tool_id: String, reason: String },
    ContextOverflow { budget: u32, used: u32 },
    CheckpointCorrupted { task_id: TaskId },
    Timeout { task_id: TaskId, elapsed: Duration },
}

enum LlmError {
    ProviderUnavailable { provider: String, status: u16 },
    RateLimited { provider: String, retry_after: Duration },
    InvalidResponse { reason: String },
    ContentFiltered { reason: String },
}

enum ToolError {
    NotFound { tool_id: String },
    ExecutionFailed { tool_id: String, message: String },
    Timeout { tool_id: String, elapsed: Duration },
    McpServerDown { server_id: String },
    CapabilityDenied { agent_id: AgentId, capability: String },
}

enum PolicyError {
    CapabilityDenied { agent_id: AgentId, capability: CapabilityRef },
    SecretAccessDenied { agent_id: AgentId, secret_ref: SecretRef },
    PartitionAccessDenied { agent_id: AgentId, partition: String },
}

enum InfraError {
    Database(sqlx::Error),
    Io(std::io::Error),
    Config(ConfigError),
}
```

All errors implement `std::error::Error` and produce structured OTEL events with the error variant as an attribute.

---

## 7. LLM integration via Rig

### 7.1 Rig as first-class dependency

Rig is the **primary** LLM integration layer. Neuromancer does not define a custom `LlmProvider` trait. ([Docs.rs][9])

**Dependency boundary:**

| Crate | rig dependency? | Reason |
|---|---|---|
| `neuromancer-core` | No | Defines traits only |
| `neuromancer-agent` | **Yes** (full) | Agent construction, execution, tool dispatch |
| `neuromancerd` | Transitive | Via `neuromancer-agent` |

### 7.2 Agent construction with rig AgentBuilder

Each sub-agent is constructed at dispatch time using rig's `AgentBuilder`:

```rust
async fn build_agent(
    config: &AgentConfig,
    model_router: &ModelRouter,
    tools: Vec<Box<dyn rig::tool::Tool>>,
    mcp_tools: Vec<(McpToolDefinition, McpPeer)>,
    system_prompt: String,
    orchestrator: Option<Arc<Orchestrator>>,  // for request_agent_assistance
) -> Result<rig::agent::Agent> {
    let model = model_router.resolve(&config.model_slot)?;
    let mut builder = model.agent(system_prompt);

    // Add policy-wrapped native tools
    for tool in tools {
        builder = builder.tool(tool);
    }

    // Add MCP tools via rig's mcp integration
    for (tool_def, peer) in mcp_tools {
        builder = builder.mcp_tool(tool_def, peer.into());
    }

    // Add collaboration tool if agent has can_request capabilities
    if !config.capabilities.can_request.is_empty() {
        if let Some(orch) = orchestrator {
            builder = builder.tool(RequestAgentAssistance::new(orch));
        }
    }

    // Add TOTP/2FA tools if agent has TOTP secrets in its ACL
    if secrets_broker.has_totp_secrets_for(&config.agent_id).await {
        builder = builder.tool(GenerateTotp::new(secrets_broker.clone()));
        builder = builder.tool(Escalate2fa::new(escalation_channel.clone()));
    }

    Ok(builder.build())
}
```

### 7.3 MCP tool integration via rig + rmcp

1. The `McpClientPool` (in `neuromancer-mcp`) maintains connections to configured MCP servers.
2. At agent construction time, allowed MCP tools are resolved from the pool.
3. Each MCP tool is wrapped via `builder.mcp_tool(tool_def, peer)`.
4. The `ToolBroker` performs policy checks before tool execution, even for MCP tools.

### 7.4 Model router

```rust
struct ModelRouter {
    slots: HashMap<String, Box<dyn CompletionModel>>,
}

impl ModelRouter {
    fn resolve(&self, slot: &str) -> Result<&dyn CompletionModel>;
}
```

Slot assignments are driven by:

1. Central TOML config defaults (`[models]` section)
2. Per-agent overrides (`[agents.<n>.models]`)
3. Per-skill hinting in frontmatter (`metadata.neuromancer.models.preferred`)

---

## 8. Skills system

### 8.1 Skill format

Neuromancer is compatible with the **Agent Skills** pattern:

* `SKILL.md` with frontmatter + instructions
* Resources in the skill directory (scripts, templates, data files)
* Multiple skill directories with configurable search paths

### 8.2 Neuromancer permissions in frontmatter

```yaml
---
name: "browser_summarize"
version: "0.1.0"
description: "Browse a page and summarize it"
metadata:
  neuromancer:
    requires:
      mcp_servers: ["playwright"]
      secrets: []
      memory_partitions: ["workspace:default"]
      script_execution: false
    models:
      preferred: "browser_model"
---
```

**Critical invariant:** Skill metadata is not sufficient to grant permissions. It is a *declaration*. The central config must authorize it. If a skill declares `mcp_servers: ["playwright"]` and the agent doesn't have `playwright` in `capabilities.mcp_servers`, the skill is rejected at load time.

### 8.3 Skill loading lifecycle

1. **Discovery:** Scan configured skill directories for `SKILL.md` files.
2. **Parse frontmatter:** Extract metadata using `gray_matter` + `yaml-rust2`.
3. **Permission validation:** Compare declared `metadata.neuromancer.requires` against the agent's capability set in config.
   * If all requirements satisfied → skill loads normally.
   * If requirements not satisfied → skill loads in **degraded mode**: instructions are injected but the agent is told (via an appended instruction line) that specific resources are unavailable. This is a warning, not a hard failure, because many skills have optional scripts while core instructions still work.
4. **Instruction injection:** Active skill instructions are appended to the agent's system prompt at construction time.
5. **Hot reload:** File watcher (`notify`) detects changes to skill directories. Modified skills are re-parsed and re-validated. Changes take effect on next agent construction (next task dispatch).

### 8.4 Script execution

Skills may include scripts in their resource directory. Script execution is a **declared, gated capability**.

**Configuration:**

```toml
[agents.planner]
capabilities.script_execution = true  # Must be explicitly enabled
```

**Runtime:**

```rust
async fn execute_skill_script(
    script_path: &Path,
    working_dir: &Path,
    injected_env: HashMap<String, String>,  // From secrets broker, per skill ACL
    timeout: Duration,
) -> Result<ScriptOutput> {
    let mut cmd = tokio::process::Command::new(script_path);
    cmd.current_dir(working_dir);
    cmd.env_clear();          // CRITICAL: scrub inherited environment
    cmd.envs(injected_env);   // Only what the secrets broker authorizes
    // ... timeout, capture stdout/stderr, return
}
```

`cmd.env_clear()` is the security invariant: no ambient environment leakage. The secrets broker decides what gets injected based on the skill's declared requirements matched against the agent's ACL.

### 8.5 Parsing frontmatter

* `gray_matter` for frontmatter extraction (supports YAML/TOML/JSON) ([Docs.rs][12])
* `yaml-rust2` as a YAML 1.2 implementation (`serde_yaml` is unmaintained) ([Docs.rs][11])

---

## 9. MCP integration

### 9.1 SDK choice

**RMCP** (`rmcp`) from the official MCP Rust SDK repository, surfaced through rig's `.mcp_tool()`. ([GitHub][5])

### 9.2 MCP server lifecycle manager

* Start/stop servers on demand (stdio child processes or remote connections)
* Health checks and restart policies
* Per-server environment injection from secrets broker (**with scrubbed base environment**)
* Per-agent allowlist filtering: agent sees only tools from allowed servers

### 9.3 Environment scrubbing for child processes

**Critical security requirement:** MCP servers spawned as child processes must NOT inherit `neuromancerd`'s environment. The daemon may have provider API keys in env vars. These would leak to every MCP child process by default.

```rust
async fn spawn_mcp_server(
    config: &McpServerConfig,
    secrets_broker: &dyn SecretsBroker,
) -> Result<McpConnection> {
    let mut cmd = tokio::process::Command::new(&config.command[0]);
    cmd.args(&config.command[1..]);
    cmd.env_clear();  // Scrub ALL inherited environment

    // Inject only authorized secrets for this specific server
    for secret_ref in &config.authorized_secrets {
        let resolved = secrets_broker.resolve_for_server(secret_ref).await?;
        cmd.env(&resolved.env_key, resolved.value.expose_secret());
    }

    // Inject minimal required env (PATH, HOME, etc.)
    cmd.env("PATH", minimal_path());
    cmd.env("HOME", config.working_dir.to_str().unwrap());

    // ... spawn, connect stdio, return McpConnection
}
```

---

## 10. Central TOML configuration

### 10.1 Design principles

* **Explicit capabilities**, no ambient permissions
* **`TriggerType` privilege boundary**: only `TriggerType::Admin` can authorize mutations
* **Hierarchical inheritance**: children inherit parent defaults, can only reduce permissions unless explicitly allowed

### 10.2 Bootstrap

* Run `neuroctl install` to bootstrap missing config/runtime directories and default prompt files (never overwriting existing files).
* XDG roots resolve under `$XDG_CONFIG_HOME/neuromancer` (or `~/.config/neuromancer`) and `$XDG_DATA_HOME/neuromancer` (or `~/.local/neuromancer`).
* Provider API keys stored under `$XDG_RUNTIME_HOME/neuromancer/provider_keys` during beta. **This is temporary and insecure; OS keychain integration is mandatory for post-beta.**
* `neuroctl install` detects configured providers and prompts for one API key per provider.
* Daemon startup fails if configured/default prompt files are missing or empty.

### 10.3 Example config

```toml
[global]
instance_id = "home-lab-01"
workspace_dir = "/var/lib/neuromancer/workspaces"
data_dir = "/var/lib/neuromancer/data"

[otel]
service_name = "neuromancer"
otlp_endpoint = "http://otel-collector:4318"

[secrets]
backend = "local_encrypted"
keyring_service = "neuromancer"
require_acl = true

[secrets.entries.github_pat]
kind = "credential"
allowed_agents = ["planner", "browser"]
allowed_mcp_servers = ["github-mcp"]

[secrets.entries.github_totp]
kind = "totp_seed"
allowed_agents = ["browser"]
totp_policy = "autonomous"
totp_digits = 6
totp_period = 30

[secrets.entries.bank_totp]
kind = "totp_seed"
allowed_agents = ["browser"]
totp_policy = "escalate"

[secrets.entries.discord_bot_token]
kind = "credential"
allowed_agents = []  # Only used by trigger manager, not agents

[secrets.entries.web_login_cookiejar]
kind = "credential"
allowed_agents = ["browser"]

[secrets.entries.admin_api_token]
kind = "credential"
allowed_agents = []  # Only used by admin API server

[memory]
backend = "sqlite"
sqlite_path = "/var/lib/neuromancer/data/memory.db"

[models]
orchestrator = { provider = "openai", model = "gpt-4o" }
planner = { provider = "openai", model = "gpt-4o" }
executor = { provider = "openai", model = "gpt-4o" }
browser = { provider = "anthropic", model = "claude-sonnet-4-5-20250929" }
extractor = { provider = "openai", model = "gpt-4o-mini" }

[mcp_servers.playwright]
kind = "child_process"
command = ["npx", "-y", "@playwright/mcp-server"]
authorized_secrets = []

[mcp_servers.filesystem]
kind = "builtin"
allowed_roots = ["/var/lib/neuromancer/workspaces"]

# --- Orchestrator ---

[orchestrator]
model_slot = "orchestrator"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"
max_iterations = 30

[orchestrator.session]
context_window_budget = 128000
truncation_strategy = "summarize"
summarize_threshold_pct = 0.75
summarizer_model_slot = "extractor"

# --- Agents ---

[agents.extractor]
description = "Extracts durable facts and summaries from task outputs"
mode = "inproc"
models.executor = "extractor"
system_prompt_path = "prompts/agents/extractor/SYSTEM.md"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:extractor"]
capabilities.can_request = []

[agents.planner]
description = "Plans multi-step tasks and coordinates execution"
mode = "inproc"
models.executor = "planner"
system_prompt_path = "prompts/agents/planner/SYSTEM.md"
capabilities.skills = ["task_manager", "code_review"]
capabilities.mcp_servers = []
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:planner"]
capabilities.can_request = ["browser", "files"]
capabilities.script_execution = false

[agents.planner.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"

[agents.browser]
description = "Web browsing, scraping, and interaction"
mode = "inproc"
models.executor = "browser"
system_prompt_path = "prompts/agents/browser/SYSTEM.md"
capabilities.skills = ["browser_summarize", "browser_clickpath"]
capabilities.mcp_servers = ["playwright"]
capabilities.secrets = ["web_login_cookiejar"]
capabilities.memory_partitions = ["workspace:default", "agent:browser"]
capabilities.can_request = []

[agents.browser.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"
circuit_breaker = { failures = 5, window = "10m" }

[agents.files]
description = "File system operations within authorized roots"
mode = "inproc"
system_prompt_path = "prompts/agents/files/SYSTEM.md"
capabilities.skills = ["fs_readwrite"]
capabilities.mcp_servers = ["filesystem"]
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:files"]
capabilities.can_request = []

# --- Discord trigger ---

[triggers.discord]
enabled = true
token_secret = "discord_bot_token"
allowed_guilds = ["123456"]
dm_policy = "allowed_users_only"

[triggers.discord.rate_limit]
per_user_per_minute = 10
per_channel_per_minute = 30
max_concurrent_tasks = 5

[[triggers.discord.channel_routes]]
channel_id = "general"
agent = "planner"
thread_mode = "per_task"
activation = "mention"

[[triggers.discord.channel_routes]]
channel_id = "browser-tasks"
agent = "browser"
thread_mode = "per_task"
activation = "all"

[triggers.discord.response]
max_message_length = 2000
overflow = "split"
use_embeds = true
auto_code_blocks = true

# --- Cron triggers ---

[[triggers.cron]]
id = "daily-issue-summary"
description = "Summarize new GitHub issues and post to #reports"
enabled = true
schedule = "0 0 9 * * *"

[triggers.cron.task_template]
agent = "planner"
instruction = "Summarize all new issues opened in the last 24 hours for repos: {{repos}}."

[triggers.cron.task_template.parameters]
repos = ["org/repo-a", "org/repo-b"]

[triggers.cron.execution]
timeout = "5m"
idempotency_key = "issue-summary-{{date}}"
dedup_window = "1h"
on_failure = "retry"
max_retries = 2

[triggers.cron.notification]
on_success = { channel = "123456789", template = "Completed: {{summary}}" }
on_failure = { channel = "admin-dm", template = "FAILED: {{error}}" }

# --- Admin API ---

[admin_api]
bind_addr = "127.0.0.1:9090"
enabled = true
bearer_token_secret = "admin_api_token"  # Required. Resolved via secrets broker.
```

### 10.4 Hot reload

File watcher (`notify`) monitors the config file. On change:

* Validate the new config fully before applying.
* Diff against current config to determine what changed.
* Agent capability changes take effect on next task dispatch (not mid-task).
* Routing rule changes take effect immediately.
* Trigger config changes restart the affected trigger source.

---

## 11. Secrets store and access controls

### 11.1 Core rules

**Rule 1 — Secrets are not LLM context.** Agents reference secrets by **handle** (e.g., `<GITHUB_TOKEN>`). The agent harness performs token replacement in tool call arguments at execution time. The plaintext value never enters the conversation context, the system prompt, or the LLM request.

**Rule 2 — Secrets are detected and redacted on output.** Handle-based injection prevents secrets from entering context via *input*, but secrets can leak through *output* channels: tool results echoing auth headers, error messages containing credentials, file reads returning config dumps. An outbound secret scanner catches these at every exit point.

### 11.2 Handle-based injection

The primary injection mechanism is token replacement in tool call arguments. When a tool call contains `<GITHUB_TOKEN>`, the harness:

1. Resolves the handle via `SecretsBroker` (ACL check, audit log).
2. Replaces the token in the argument string with the plaintext value.
3. Executes the tool with the resolved arguments.
4. The LLM never sees the plaintext — it only ever sees and produces handle tokens.

For MCP child processes, injection is via scrubbed environment variables (see §9.3). The two paths serve different use cases: handle replacement for tool call arguments, env injection for server processes that read credentials from their environment at startup.

### 11.3 Outbound secret scanner

The scanner prevents secret exfiltration through output channels. It uses Aho-Corasick multi-pattern matching for O(n) scanning regardless of the number of active secrets.

```rust
use aho_corasick::AhoCorasick;
use arc_swap::ArcSwap;
use secrecy::{ExposeSecret, SecretString};

struct SecretScanner {
    /// Built from all active secret values + common transformations
    automaton: AhoCorasick,
    /// Map from match pattern index back to secret_id for audit logging
    pattern_to_secret: Vec<SecretId>,
}

impl SecretScanner {
    fn build(secrets: &[(SecretId, SecretString)]) -> Self {
        let mut patterns = Vec::new();
        let mut pattern_to_secret = Vec::new();

        for (id, value) in secrets {
            let raw = value.expose_secret();
            // Skip very short secrets (high false positive rate)
            if raw.len() < 8 { continue; }

            // Raw value
            patterns.push(raw.to_string());
            pattern_to_secret.push(id.clone());

            // Base64 encoded
            patterns.push(base64_encode(raw));
            pattern_to_secret.push(id.clone());

            // URL encoded
            patterns.push(urlencoding::encode(raw).to_string());
            pattern_to_secret.push(id.clone());
        }

        SecretScanner {
            automaton: AhoCorasick::new(&patterns).unwrap(),
            pattern_to_secret,
        }
    }

    /// Scan text for any known secret values. Returns matched secret IDs.
    fn scan(&self, text: &str) -> Vec<SecretId> {
        self.automaton
            .find_iter(text)
            .map(|m| self.pattern_to_secret[m.pattern()].clone())
            .collect()
    }

    /// Redact all matched secrets, replacing with [REDACTED:<secret_id>]
    fn redact(&self, text: &str) -> String {
        // Build replacement strings: [REDACTED:github_token], etc.
        self.automaton.replace_all(text, &self.replacement_strings())
    }
}
```

**The scanner is stored behind `ArcSwap<SecretScanner>`** so it can be rebuilt atomically when secrets are added, rotated, or deleted without blocking in-flight scans.

**Four chokepoints where the scanner runs:**

1. **Post-tool-call:** Before a tool result enters the conversation context. If a secret is detected, redact it and emit an OTEL warning span with `secret.leak_detected` attribute. The agent sees `[REDACTED:github_token]` instead of the real value.
2. **Pre-outbound-message:** Before any response is sent to Discord or another trigger channel. Redact and warn.
3. **Pre-log:** Before writing to OTEL span attributes or conversation logs. Redact sensitive content.
4. **Pre-memory-write:** Before `MemoryStore.put()`. Redact and warn.

**Performance:** Building the automaton is O(total pattern bytes). Scanning is O(input length). For a personal agent with dozens of secrets and typical message sizes, this is negligible — microseconds per scan.

### 11.4 Storage backend

* **Data store**: SQLite (via `sqlx`) for metadata + ciphertext blobs ([GitHub][14])
* **Encryption at rest**: `age` for encrypting individual secret payloads ([Docs.rs][15])
* **Master key storage**: OS keychain via `keyring` (macOS Keychain, Windows Credential Manager, Linux Secret Service) ([Docs.rs][16])
* **In-memory handling**: `secrecy::SecretBox/SecretString` + `zeroize` to prevent accidental serialization and ensure memory clearing ([Docs.rs][17])

**Startup flow:**

1. Resolve master key from OS keychain via `keyring`.
2. Open secrets SQLite database.
3. Decrypt secret payloads into `SecretString` values held in memory.
4. Build the `SecretScanner` automaton from all active secret values.
5. Secrets broker is ready to serve handle resolution requests.

**Backend extensibility:** The `SecretsBroker` trait allows alternative backends (1Password via `op` CLI, HashiCorp Vault, etc.) as post-beta implementations. Beta ships `local_encrypted` (SQLite + age + keychain) only.

```toml
[secrets]
backend = "local_encrypted"
keyring_service = "neuromancer"
require_acl = true
```

### 11.5 Secret kinds and ACL model

Secrets support multiple kinds, each with specific handling:

```rust
enum SecretKind {
    /// API keys, bearer tokens, passwords.
    /// Injected via handle replacement or env var.
    Credential,

    /// TOTP seed (otpauth:// secret).
    /// Used by the generate_totp tool. See §11.7.
    TotpSeed {
        /// Whether the agent can generate codes autonomously
        /// or must escalate to the user.
        policy: TotpPolicy,
        /// TOTP parameters (digits, period, algorithm).
        params: TotpParams,
    },

    /// TLS certificates, SSH keys, etc.
    /// Injected via file mount into tool working directory.
    CertificateOrKey,
}

enum TotpPolicy {
    /// Agent generates TOTP codes autonomously via generate_totp tool.
    Autonomous,
    /// Agent must escalate to user via trigger channel for the code.
    Escalate,
}
```

**Implementation note (beta codebase):**

* `browser_session` remains supported as a beta extension for browser cookie/session write-back (`store_session` flow).
* `totp_seed` config parsing/modeling is currently stubbed for SDS compatibility; end-to-end runtime TOTP tool integration is still deferred.

Each secret has:

* `kind`: `Credential` | `TotpSeed` | `CertificateOrKey`
* `allowed_agents`: which agents can resolve this handle
* `allowed_skills`: (optional) which skills can use this secret
* `allowed_mcp_servers`: (optional) which MCP servers receive this in their environment
* `allowed_injection_modes`: env var, header, file mount, handle replacement
* `ttl` / `rotation_metadata`: expiry and rotation tracking

### 11.6 Auditing

Every secret access event emits an OTEL span with:

* `secret_id`, `agent_id`, `task_id`, `tool_id`, `reason`
* `outcome`: granted / denied / leaked_and_redacted
* Hashed callsite metadata

The `leaked_and_redacted` outcome is emitted when the outbound scanner detects and redacts a secret in output. This creates an audit trail for exfiltration attempts even when they're caught.

### 11.7 OTP / 2FA handling

When an agent encounters a 2FA challenge from an external service (e.g., a browser agent logging into a web service), the behavior depends on the secret's `TotpPolicy`:

#### Autonomous TOTP generation

For services where the agent should have autonomous access, the TOTP seed is stored in the secrets broker and the agent uses a `generate_totp` tool:

```rust
/// Tool available to agents with TOTP secrets in their ACL.
struct GenerateTotp {
    secrets_broker: Arc<dyn SecretsBroker>,
}

impl Tool for GenerateTotp {
    type Input = TotpRequest;
    type Output = TotpCode;

    async fn call(&self, ctx: &AgentContext, input: TotpRequest) -> Result<ToolResult> {
        // 1. Resolve the TOTP seed via secrets broker (ACL check + audit)
        let seed = self.secrets_broker
            .resolve_handle_for_tool(ctx, input.secret_ref, SecretUsage::TotpGeneration)
            .await?;

        // 2. Check that the secret's policy allows autonomous generation
        match seed.totp_policy() {
            TotpPolicy::Autonomous => {},
            TotpPolicy::Escalate => {
                return Err(ToolError::ExecutionFailed {
                    tool_id: "generate_totp".into(),
                    message: "This secret requires human escalation for 2FA. \
                              Use escalate_2fa instead.".into(),
                });
            }
        }

        // 3. Generate the TOTP code (totp-rs crate)
        let code = totp_rs::TOTP::from_secret(seed.expose_secret())?
            .generate_current()?;

        // 4. Return the code as a tool result
        //    Note: the 6-digit code is ephemeral and short-lived (30s),
        //    but the scanner will still check output channels.
        Ok(ToolResult::text(code))
    }
}
```

**Important:** The TOTP *seed* never enters LLM context. The generated 6-digit *code* does appear in the tool result (the agent needs to type it somewhere), but codes expire in 30 seconds and the scanner's pattern matching is tuned for seeds, not ephemeral codes.

#### Human-in-the-loop escalation

For sensitive services, the agent detects the 2FA challenge and escalates:

```rust
struct Escalate2fa;

impl Tool for Escalate2fa {
    async fn call(&self, ctx: &AgentContext, input: Escalate2faRequest) -> Result<ToolResult> {
        // Transition the task to WaitingForInput
        // Send escalation message to the user via trigger channel:
        //   "Service X is asking for a 2FA code. Please reply with the code."
        // Block until user responds or timeout
        // Return the user-provided code as a tool result
    }
}
```

This uses the existing `WaitingForInput` state and escalation flow (§6.3, §6.5 Path 2). The agent's task is paused, the user provides the code via Discord or CLI, and the agent continues.

#### Push-based approval

For services with push-notification 2FA (Duo, Microsoft Authenticator), the agent escalates with a different message: "I've triggered a push notification on Service X, please approve it on your device." The agent then polls/waits for the login to succeed rather than expecting a code in response.

#### Configuration

```toml
[secrets.entries.github_totp]
kind = "totp_seed"
allowed_agents = ["browser"]
totp_policy = "autonomous"
totp_digits = 6
totp_period = 30
totp_algorithm = "sha1"  # sha1 | sha256 | sha512

[secrets.entries.bank_totp]
kind = "totp_seed"
allowed_agents = ["browser"]
totp_policy = "escalate"

[secrets.entries.github_pat]
kind = "credential"
allowed_agents = ["planner", "browser"]
allowed_mcp_servers = ["github-mcp"]
```

### 11.8 Admin API token

The admin API bearer token is itself a secret managed by the secrets broker. On first `neuroctl install`, a random token is generated, encrypted, and stored. The daemon resolves it at startup. This means the admin API auth secret follows the same ACL and audit path as every other secret.

---

## 12. Memory

### 12.1 Partitioning model (beta)

Beta supports two partition types:

* `agent:<agent_id>` — private long-term storage per agent
* `workspace:default` — shared project memory, readable/writable by agents with authorization

Each agent is allowed to read/write only its authorized partitions.

### 12.2 Storage

SQLite via `sqlx`. ([GitHub][14])

### 12.3 Memory record schema

```sql
CREATE TABLE memory_items (
    id TEXT PRIMARY KEY,
    partition TEXT NOT NULL,
    kind TEXT NOT NULL,       -- 'summary', 'fact', 'artifact', 'tool_result'
    content TEXT NOT NULL,
    tags TEXT,                -- JSON array
    created_at TEXT NOT NULL,
    expires_at TEXT,
    source TEXT,              -- 'discord', 'cron', 'tool:<id>', 'system0', 'extractor'
    sensitivity TEXT NOT NULL DEFAULT 'private'  -- 'public', 'private', 'secret_ref_only'
);

CREATE INDEX idx_partition_kind ON memory_items(partition, kind);
CREATE INDEX idx_partition_created ON memory_items(partition, created_at DESC);
CREATE INDEX idx_expires ON memory_items(expires_at) WHERE expires_at IS NOT NULL;
```

### 12.4 "Never store raw secrets" rule

Memory may store secret handles and references, redacted snippets. Never plaintext secrets. Enforced at two layers: the `PolicyEngine` rejects writes containing handle tokens, and the outbound secret scanner (§11.3) redacts any plaintext secret values at the `MemoryStore.put()` chokepoint before they reach storage.

### 12.5 Memory usage during agent execution

**On task initialization (read):**

1. Query `workspace:default` for items matching the task's topic/tags (relevance by recency + tag overlap).
2. Query `agent:<agent_id>` for the agent's own long-term context.
3. Inject retrieved items into the `EphemeralSession` as system messages, respecting token budget. Most relevant first, truncated if budget exceeded.

**On task completion (write via extraction agent):**

1. System0 dispatches the task summary to the extraction agent.
2. Extraction agent generates durable facts, preferences, and patterns.
3. Extraction agent writes to `workspace:default` with `kind: fact` or `kind: summary`.
4. Extraction agent writes agent-specific learnings to `agent:<agent_id>`.

**Explicit agent tools:**

* **`remember`**: Explicitly store a fact or note. Writes to `agent:<agent_id>` or `workspace:default` per policy.
* **`recall`**: Search memory by query string. Returns matching items from authorized partitions.

### 12.6 Extraction agent

The extraction agent is a dedicated sub-agent whose job is knowledge distillation:

* Receives completed task summaries and outputs
* Extracts durable facts, preferences, and patterns
* Writes them to the appropriate MemoryStore partition with proper `kind` tags
* Reconciles conflicting facts when possible
* Runs on a cheap model (`extractor` slot, e.g., `gpt-4o-mini`)
* Capability set: `memory_partitions: ["workspace:default", "agent:*"]`, no MCP, no secrets, no skills

This replaces OpenClaw's "silent NO_REPLY pre-compaction flush" with a first-class agent that produces structured, high-quality memory entries rather than a side-channel hack.

### 12.7 Crate separation

```
neuromancer-core/src/memory.rs       # trait MemoryStore, MemoryItem, MemoryQuery, etc.
neuromancer-memory-simple/src/lib.rs  # SqliteMemoryStore: impl MemoryStore
neuromancer-memory-simple/migrations/ # SQLite schema migrations
```

---

## 13. Trigger system

### 13.1 Trigger manager contract

```rust
struct TriggerEvent {
    trigger_id: String,
    occurred_at: DateTime<Utc>,
    principal: Principal,
    payload: TriggerPayload,
    metadata: TriggerMetadata,
}
```

The Trigger Manager owns a `tokio::sync::mpsc::Sender<TriggerEvent>` and sends events to the orchestrator's main loop.

### 13.2 Discord trigger

**Crate:** `twilight` ([Docs.rs][18])

#### Configuration

```rust
struct DiscordTriggerConfig {
    token_secret: SecretRef,
    allowed_guilds: Vec<GuildId>,
    channel_routes: Vec<ChannelRoute>,
    dm_policy: DmPolicy,
    rate_limit: RateLimitConfig,
    response: DiscordResponseConfig,
}

struct ChannelRoute {
    channel_id: ChannelId,
    agent: AgentId,
    thread_mode: ThreadMode,
    activation: ActivationRule,
}

enum ThreadMode { PerTask, Inherit, Disabled }
enum ActivationRule { All, Mention, Prefix(String), Reply }
enum DmPolicy { Open, AllowedUsersOnly, Disabled }
```

#### Message-to-task mapping

1. Extract message content, attachments, and thread context.
2. If attachments present: download to task-scoped workspace directory.
3. Apply channel_routes policy checks.
4. If `thread_mode` is `PerTask`: create a Discord thread.
5. Construct `TriggerEvent` and send to orchestrator.

#### Response delivery

```rust
struct DiscordResponseConfig {
    max_message_length: usize,   // default: 2000
    overflow: OverflowStrategy,  // Split | FileAttachment | Truncate
    use_embeds: bool,
    auto_code_blocks: bool,
}
```

#### Rate limiting

```rust
struct RateLimitConfig {
    per_user_per_minute: u32,      // default: 10
    per_channel_per_minute: u32,   // default: 30
    max_concurrent_tasks: u32,     // default: 5
}
```

Sliding window, in-memory. When exceeded, react with rate-limit emoji and drop the task.

#### Connection lifecycle

* On startup: connect to Discord gateway via twilight, register for `MESSAGE_CREATE`.
* On disconnect: automatic reconnection with exponential backoff.
* On shutdown: gracefully close gateway. In-flight tasks continue; new events dropped.

### 13.3 Cron trigger

**Crate:** `tokio-cron-scheduler` ([Docs.rs][20])

#### Configuration

Each cron trigger is a named job with a schedule, task template, execution parameters, and notification rules. See §10.3 for the full TOML format.

#### Template rendering

`minijinja` for rendering. Available variables:

* All keys from `task_template.parameters`
* `{{date}}` — current date (YYYY-MM-DD)
* `{{datetime}}` — current datetime (ISO 8601)
* `{{job_id}}` — the cron job ID

#### Execution flow

1. `tokio-cron-scheduler` fires at scheduled time.
2. Render instruction template.
3. Compute idempotency key (if configured).
4. Check dedup: skip if matching key exists in non-terminal state within `dedup_window`.
5. Create Task and enqueue via orchestrator.
6. On completion/failure: render and send notification.

---

## 14. Observability via OpenTelemetry

### 14.1 Export

`opentelemetry-otlp` supports OTLP over gRPC/HTTP. ([Docs.rs][21])

### 14.2 Required instrumentation

Each task run emits spans:

* `trigger.receive`
* `task.create`
* `orchestrator.turn`
* `agent.dispatch`
* `agent.state_transition` (from → to state)
* `llm.call` (model id, token counts, latency)
* `tool.call` (tool id, MCP server, skill id)
* `agent.collaboration` (caller agent, target agent, task id)
* `secret.access` (handle id only, no value)
* `secret.leak_detected` (secret id, chokepoint, redacted — emitted by outbound scanner)
* `secret.totp_generated` (secret id, policy: autonomous/escalated)
* `memory.query` / `memory.put`
* `policy.decision` (allow/deny + reason code)
* `remediation.action` (what the orchestrator decided)

### 14.3 Log redaction requirements

* Never log raw prompts containing secrets
* Redact configured patterns
* Emit structured events rather than dumping payloads

---

## 15. Security model

### 15.1 Identity + least privilege

* Every agent has an identity (`agent_id`)
* Every external capability is explicit and revocable
* Permissions are specific, revocable, continuously enforced

### 15.2 Policy engine (beta: static gates only)

Beta implements static gates: allow/deny by capability as defined in TOML config. The `PolicyEngine` trait supports stacked gates, but beta only ships the static implementation.

**Enforcement is redundant:** Policy is checked both at the dispatch level (tool class routing) AND at the individual tool implementation level. A single `if trigger != Admin { deny }` at dispatch is one refactor away from a bypass; defense in depth requires both layers.

### 15.3 TriggerType privilege boundary

* Mutations hard-fail unless the current turn trigger is `TriggerType::Admin`.
* Only CLI-originated turns carry `TriggerType::Admin`.
* Non-admin sources may inspect and query but cannot authorize mutations.
* Enforcement at **both** dispatch routing AND individual tool implementations.

### 15.4 Admin API transport auth

The admin API requires a bearer token even in beta:

```
Authorization: Bearer <token>
```

The token is managed by the secrets broker. This prevents any local process from calling admin endpoints without authorization — the exact class of vulnerability that led to OpenClaw's CVE-2026-25253.

### 15.5 Secret protection: two-layer model

Secret protection operates at two layers:

1. **Input layer (handle injection):** Secrets never enter LLM context. Agents use handle tokens (`<SECRET_NAME>`), and the harness replaces them at tool execution time. MCP child processes receive secrets via scrubbed environment injection (§9.3).

2. **Output layer (exfiltration scanner):** Aho-Corasick multi-pattern scanner runs at four chokepoints: post-tool-call, pre-outbound-message, pre-log, and pre-memory-write. Detects and redacts any secret value that leaks through tool output, error messages, file reads, or command echoes. See §11.3.

### 15.6 MCP child process environment scrubbing

All MCP servers spawned as child processes have their environment scrubbed via `cmd.env_clear()`. Only secrets explicitly authorized by the broker are injected. See §9.3.

---

## 16. Admin API

The admin surface is localhost-only, bearer-token-authenticated, and centered on JSON-RPC 2.0.

**HTTP routes:**

| Method | Path | Description |
|---|---|---|
| `POST` | `/rpc` | JSON-RPC request dispatch |
| `GET` | `/admin/health` | Convenience health endpoint |
| `POST` | `/admin/config/reload` | Convenience config reload endpoint |

### 16.1 JSON-RPC method set

| Method name | Description |
|---|---|
| `admin.health` | Health/uptime/version status |
| `admin.config.reload` | Trigger config reload |
| `orchestrator.turn` | Submit one orchestrator admin turn |
| `orchestrator.runs.list` | List delegated runs |
| `orchestrator.runs.get` | Get delegated run by `run_id` |
| `orchestrator.context.get` | Get projected System0 conversation messages |
| `orchestrator.threads.list` | List thread summaries (system + subagent) |
| `orchestrator.threads.get` | Paginated thread event read |
| `orchestrator.stats.get` | Aggregate orchestrator stats |

### 16.2 Protocol behavior

* JSON-RPC version must be `2.0`.
* Single-request payloads only (batch rejected).
* Notifications not supported (`id` required).
* All requests require `Authorization: Bearer <token>` header.

---

## 17. Daemon lifecycle

### 17.1 Startup sequence

1. Load and validate TOML config.
2. Initialize OTEL tracing pipeline.
3. Open SQLite databases (memory, secrets, task queue, System0 conversation log).
4. Initialize Secrets Broker (unlock master key from OS keychain).
5. Initialize MCP Client Pool (start configured MCP servers with scrubbed environments).
6. Build Agent Registry (validate agent configs, pre-resolve model slots).
7. Load and validate skills for each agent.
8. Start Trigger Manager (connect to Discord gateway, schedule cron jobs).
9. Recover in-flight tasks: reload tasks in `Queued`/`Dispatched`/`Running` states from SQLite.
10. Start Admin API HTTP server (with bearer token auth).
11. Enter Orchestrator main loop (load System0 persistent session).

### 17.2 Signal handling

| Signal | Behavior |
|---|---|
| `SIGTERM` | Graceful shutdown: stop accepting new triggers, drain in-flight tasks (with timeout), persist checkpoints, exit. |
| `SIGINT` | Same as SIGTERM (for interactive use). |
| `SIGHUP` | Hot reload: re-read TOML config, apply changes per §10.4 rules. |

### 17.3 Graceful shutdown sequence

1. Stop Trigger Manager (disconnect Discord, stop cron scheduler).
2. Stop accepting new tasks in the queue.
3. Wait for in-flight tasks to complete (up to `shutdown_timeout`, default: 30s).
4. For tasks that don't complete in time:
   * Persist their current checkpoint to SQLite.
   * Mark them as `Suspended` with reason "daemon shutdown".
5. Close MCP server connections (send graceful shutdown to child processes).
6. Flush System0 persistent session to SQLite.
7. Flush OTEL spans.
8. Close SQLite databases.
9. Exit with code 0.

### 17.4 Restart recovery

On next startup:

1. Find tasks in `Suspended` state with reason "daemon shutdown".
2. For tasks with valid checkpoints: resume from checkpoint.
3. For tasks without checkpoints or corrupted: mark as `Failed` with `CheckpointCorrupted`.

---

## 18. Crate selection

### Daemon/runtime fundamentals

* **Async runtime:** `tokio`
* **HTTP server (admin):** `axum` ([Docs.rs][23])
* **TLS:** `rustls` ([Docs.rs][24])

### LLM integration

* **LLM framework:** `rig-core` (mandatory) ([Docs.rs][9])

### Protocols

* **MCP:** `rmcp` official SDK, surfaced through rig's `.mcp_tool()` ([GitHub][5])

### Observability

* **OTLP exporter:** `opentelemetry-otlp` ([Docs.rs][21])

### Storage

* **DB:** `sqlx` (async, SQLite) ([GitHub][14])

### Secrets

* **In-memory hygiene:** `secrecy` + `zeroize` ([Docs.rs][17])
* **OS keychain:** `keyring` ([Docs.rs][16])
* **At-rest encryption:** `age` ([Docs.rs][15])
* **Exfiltration scanner:** `aho-corasick` (multi-pattern matching, O(n) scan)
* **Atomic scanner swap:** `arc-swap` (lock-free pointer swap for scanner rebuilds)
* **TOTP generation:** `totp-rs` (TOTP/HOTP code generation from stored seeds)

### Triggers

* **Discord:** `twilight` ([Docs.rs][18])
* **Cron:** `tokio-cron-scheduler` ([Docs.rs][20])

### Skills parsing

* **Frontmatter:** `gray_matter` ([Docs.rs][12])
* **YAML:** `yaml-rust2` ([Docs.rs][11])

### Template rendering

* **Templates:** `minijinja` ([Docs.rs][25])

---

## 19. Crate layout

```
neuromancer/
  Cargo.toml                      # workspace root
  neuromancer-core/               # Core traits: ToolBroker, MemoryStore, SecretsBroker,
                                  #   PolicyEngine, error types, shared types.
                                  #   NO rig dependency. NO sqlx dependency.
  neuromancer-agent/              # Agent execution runtime: state machine, rig Agent
                                  #   construction, PersistentSession, EphemeralSession,
                                  #   model router, request_agent_assistance tool.
                                  #   Depends on: neuromancer-core, rig-core.
  neuromancerd/                   # Daemon binary: config loading, signal handling,
                                  #   lifecycle management, admin API server (bearer auth).
                                  #   Depends on: all crates.
  neuromancer-mcp/                # MCP client pool: wraps rmcp, manages server
                                  #   lifecycle, health checks, env scrubbing.
  neuromancer-skills/             # Skill loading, frontmatter parsing, permission
                                  #   validation, degraded mode, script runner.
  neuromancer-triggers/           # Trigger manager, Discord trigger (twilight),
                                  #   cron trigger (tokio-cron-scheduler).
  neuromancer-memory-simple/      # SQLite MemoryStore implementation.
                                  #   Depends on: neuromancer-core, sqlx.
  neuromancer-secrets/            # Secrets broker: encrypted store, ACL enforcement,
                                  #   handle resolution, injection.
```

---

## 20. Implementation priority (critical path)

These three items are the highest-risk and should be built first:

1. **System0 persistent session management**
   This is the thing you interact with every day. Get the conversation log, context window loading, and summarization/truncation loop working early. It affects everything downstream.

2. **`request_agent_assistance` as a tool**
   This is the mechanical heart of the swarm. Two agents, one calls the other, result comes back. If this works, the system works. Test with a simple planner→browser delegation.

3. **Secrets broker under real usage**
   The theory is clean, but the first time you use an MCP server that expects `GITHUB_TOKEN` in its environment, you'll discover edge cases in scrubbed-environment child process spawning. Hit these early.

### Full milestone sequence

1. Repo + crate layout + workspace Cargo.toml
2. `neuromancer-core`: traits, error types, shared types
3. TOML config + validation + hot reload (in `neuromancerd`)
4. OTEL tracing pipeline
5. `neuromancer-secrets`: keyring master key, age-encrypted payloads, ACL enforcement
6. `neuromancer-memory-simple`: partitioned SQLite schema, basic query
7. `neuromancer-mcp`: MCP client pool using rmcp, **env scrubbing**
8. `neuromancer-agent`: rig-based agent runtime, state machine, PersistentSession, EphemeralSession, `request_agent_assistance`
9. `neuromancer-triggers`: cron + Discord
10. `neuromancer-skills`: skill loader + permission validation + degraded mode + script runner
11. `neuromancerd`: daemon lifecycle, admin API (with bearer auth), integration

---

## 21. Open questions (intentionally deferred)

* Should "file tools" be a built-in tool surface or only via MCP servers? (Bias: built-in restricted FS tool for post-beta, plus MCP support.)
* Should the Admin API support WebSocket subscriptions for real-time status, or is polling sufficient?
* Maximum conversation history size before mandatory truncation? (Current: per-agent configurable, default 128k tokens.)
* Should cron job definitions be splittable across multiple TOML files?
* Extraction agent: should it run automatically after every task, or only when System0 decides? (Bias: automatic with configurable filter on task priority/duration.)
* Cross-agent memory sharing beyond `workspace:default`: what access patterns emerge from real usage? (Defer design until beta usage data exists.)

---

[1]: https://github.com/openclaw/openclaw/issues/11202
[2]: https://snyk.io/blog/openclaw-skills-credential-leaks-research/
[3]: https://github.com/openclaw/openclaw/issues/10864
[4]: https://1password.com/blog/from-magic-to-malware-how-openclaws-agent-skills-become-an-attack-surface
[5]: https://github.com/modelcontextprotocol/rust-sdk
[9]: https://docs.rs/rig-core
[11]: https://docs.rs/yaml-rust2/latest/yaml_rust2/
[12]: https://docs.rs/gray_matter
[14]: https://github.com/launchbadge/sqlx
[15]: https://docs.rs/crate/age/latest
[16]: https://docs.rs/keyring
[17]: https://docs.rs/secrecy
[18]: https://docs.rs/twilight/latest/x86_64-pc-windows-msvc/twilight/
[20]: https://docs.rs/crate/tokio-cron-scheduler/latest
[21]: https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/
[23]: https://docs.rs/axum/latest/axum/
[24]: https://docs.rs/rustls
[25]: https://docs.rs/minijinja
