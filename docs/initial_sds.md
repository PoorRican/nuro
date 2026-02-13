# Neuromancer v0.1-alpha System Design Specification

## 0. Status and scope

**Status:** Active v0.1-alpha runtime specification.
**Revision:** v0.1-alpha-r1 — System0 CLI runtime with prompt-path driven orchestrator/agent prompts.
**Scope:** A Rust daemon ("Neuromancer") that provides a **System0 LLM orchestrator** supervising isolated **rig-powered sub-agents**, using:

* **Agent Skills** for instruction bundles
* **MCP** for external tools/services (via rmcp, surfaced through rig's `.mcp_tool()`)
* **A2A** for agent-to-agent delegation and isolation boundaries
* **OpenTelemetry** for end-to-end observability

No new "plugin protocol" is introduced: external extensibility is primarily via **MCP servers** and **A2A peers**, with **skills** as the "instruction + packaging" layer.

**Key architectural invariant:** Public ingress is `orchestrator.turn`. The System0 orchestrator itself is a rig-backed LLM agent that mediates user/admin turns, delegates work, and maintains single-session history through `neuromancer-agent`.

**v0.1-alpha directives (authoritative):**
* No backward compatibility for removed RPC/CLI task/message surfaces.
* Prompt configuration is file-path based (`system_prompt_path`).
* Prompt files must exist and be non-empty markdown files at startup.
* Setup is explicit via `neuroctl install` (optional `--config <path>` override; no startup auto-bootstrap).

---

## 1. Motivation and problem statement

Neuromancer is explicitly designed around failures we've now repeatedly seen in the wild:

* **Secrets leaking into model context and logs**: there are reports of resolved provider API keys being serialized into the LLM prompt context on every turn in OpenClaw gateway setups—meaning keys can leak cross-provider and persist in provider logs. ([GitHub][1])
* **"Leaky skills" as a systemic pattern**: security research found a non-trivial fraction of marketplace skills that *instruct the agent* to mishandle secrets/PII, forcing API keys/passwords/credit-card data into the LLM context window or plaintext logs—i.e., not "a bug," but a predictable outcome of the architecture. ([Snyk][2])
* **Long-running instability and process leakage**: reports of orphaned child processes accumulating over 24–48 hours, memory pressure peaking in the tens of GB, and degraded Discord processing latency. ([GitHub][3])
* **Supply chain reality**: credible practitioners are now converging on the same missing ingredients—provenance, mediated execution, and **specific + revocable permissions** tied to identity. ([1Password][4])

**Neuromancer v0.5** is therefore defined by: **least privilege**, **isolation**, **brokered secrets**, and **observable operations**, while staying compatible with the ecosystems forming around MCP, A2A, and skills.

---

## 2. Goals and non-goals

### 2.1 Goals

1. **Daemon-first architecture**
   A resilient, long-running service with explicit lifecycle management and resource controls (no "CLI app that you keep alive with prayers").

2. **System0 orchestration, LLM-powered agents**
   The orchestrator is a first-class LLM runtime (System0) that mediates user/admin turns, policy, and delegation. It handles `orchestrator.turn` ingress directly, maintains one ongoing conversation context, and delegates complex execution to sub-agents via controlled tooling.

3. **No new plugin protocol**

   * Tools/services → **MCP**
   * Remote agents → **A2A**
   * Instruction packaging → **Skills**
     The "tool surface" is unified internally, but external "plugins" are expected to be MCP servers or A2A agents.

4. **Security by construction**

   * Secrets never go into LLM context by default
   * Capabilities are explicit and auditable (skills, MCP servers, A2A peers, memory partitions, filesystem roots, network egress)
   * Sub-agents run isolated (prefer container), with a clear request path for out-of-bounds capabilities

5. **Full observability**

   * Structured traces/spans for: trigger → task → LLM calls → tool calls → secret accesses → memory reads/writes
   * Export via OpenTelemetry OTLP

6. **Admin API for runtime introspection**
   Localhost HTTP endpoints for orchestrator turns, run inspection, agent health, and cron management.

### 2.2 Non-goals (v0.1-alpha)

* A public skill marketplace / package manager (v0.5 supports local skill packs; supply-chain hardening is *foundation work*).
* Full multi-tenant enterprise RBAC UI.
* A polished web UI (v0.5 exposes a local admin API; a GUI is not required).

---

## 3. High-level architecture

Neuromancer is split into **control plane** and **data plane**. The control plane is System0, implemented as an LLM agent runtime with policy-gated control-plane tooling.

### 3.1 Control plane: `neuromancerd` + System0 orchestrator

The daemon process hosts **System0**, which mediates user/admin turns and delegates execution to sub-agents. The orchestrator:

* Loads **central TOML config**
* Runs **Trigger Manager** (Discord + cron)
* Maintains **Secrets Broker**, **Memory Store**, **MCP Client Pool**, **A2A Registry**
* Accepts public ingress from `orchestrator.turn` and processes one queued turn at a time
* Supervises sub-agent runtimes (in-process, subprocess, or container)
* Manages delegated-run tracking and audit trail
* Facilitates cross-agent communication and remediation
* Provides an **Admin API** on localhost

System0 itself is an LLM agent and can reason/plan, but outbound actions are constrained by an allowlisted tool broker and trigger/capability policy.

```rust
struct Orchestrator {
    config: Arc<NeuromancerConfig>,
    input_message_queue: InputMessageQueue,
    delegated_run_registry: DelegatedRunRegistry,
    system0_runtime: AgentRuntime,
    global_session_id: AgentSessionId,
    memory_store: Arc<dyn MemoryStore>,
    secrets_broker: Arc<dyn SecretsBroker>,
    mcp_pool: McpClientPool,
    policy_engine: Arc<dyn PolicyEngine>,
}

impl Orchestrator {
    /// Enqueue one public inbound turn from CLI/admin RPC.
    async fn orchestrator_turn(&self, message: String) -> Result<OrchestratorTurnResult>;

    /// Execute one System0 turn against the global agent session.
    async fn process_turn(&self, turn: InputTurn) -> Result<OrchestratorTurnResult>;

    /// Read-only delegated run introspection.
    async fn list_runs(&self) -> Result<Vec<DelegatedRun>>;
    async fn get_run(&self, run_id: String) -> Result<DelegatedRun>;
}
```

### 3.2 Data plane: sub-agent runtimes (rig Agents do ALL reasoning)

Each sub-agent runtime wraps a **rig Agent** configured with a strict **capability set**:

* Allowed skills
* Allowed MCP servers/tools
* Allowed A2A peers
* Allowed secrets (by handle)
* Allowed memory partitions
* Allowed filesystem roots
* Allowed outbound network policy

Sub-agents are the **only** components that call LLMs. They are constructed using rig's `AgentBuilder`, given tools via rig's tool trait and `.mcp_tool()`, and execute via `agent.chat()`.

### 3.3 Architecture diagram

```
                        +-------------------------------+
Discord / Cron -------->|     neuromancerd (control)     |<-------- Admin API
  Triggers              |  Orchestrator (System0 LLM)     |         (localhost)
                        |  - turn ingress queue          |
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
          | model: gpt-4o  | | model: gpt-4o  | | model: gpt-4o-m |
          | skills: [plan] | | skills: [web]  | | skills: [fs]     |
          +-------+--------+ +-------+--------+ +--------+--------+
                  |                   |                    |
                  | MCP (via rig)     | MCP (via rig)      | MCP (via rig)
                  v                   v                    v
           [MCP servers...]   [Playwright MCP]    [FS MCP / built-in]
```

**Key difference from v0.5-r1:** There is no deterministic global routing layer in v0.1-alpha. System0 handles turns directly as the orchestrator.

---

## 4. Protocol choices (why "no new plugin protocol" works)

### 4.1 MCP for tools

Neuromancer integrates tools via **MCP**, using the **official Rust SDK** (`rmcp`) which provides protocol implementation and supports building clients/servers on Tokio. ([GitHub][5])

Tools are surfaced to rig agents via rig's `.mcp_tool()` integration, which wraps rmcp tool definitions as rig-compatible tools.

This is the primary path for:

* Browser automation servers
* File tooling servers
* External SaaS/API connectors
* "Sidecar" services (indexers, search, etc.)

### 4.2 A2A for delegation and isolation

Neuromancer uses **A2A** as the standard for sub-agent delegation and isolation boundaries.

Key A2A elements Neuromancer relies on:

* **Agent discovery via Agent Card** at `/.well-known/agent-card.json` ([A2A Protocol][6])
* **HTTP+JSON binding** endpoints like `POST /message:send`, `POST /message:stream` (SSE), `GET /tasks/{id}`, etc. ([A2A Protocol][6])
* Content type `application/a2a+json` and versioning headers ([A2A Protocol][6])
* Strong guidance on permission failures and non-leaky errors (helpful to model Neuromancer's "capability missing" UX) ([A2A Protocol][6])
* Agent Cards may be signed with JWS; clients should verify when present ([A2A Protocol][6])

**A2A task state mapping** to Neuromancer's internal states (see §6.5):
* `SubAgentReport::Progress` → A2A `working`
* `SubAgentReport::InputRequired` → A2A `input_required`
* `SubAgentReport::Completed` → A2A `completed`
* `SubAgentReport::Failed` → A2A `failed`

### 4.3 Skills as instruction packaging

Skills remain critical, but they are treated as **declarative bundles**:

* metadata (what it is)
* instructions (what the agent should do)
* resources (scripts, templates, files)
* explicit permissions requirements (Neuromancer addition)

OpenClaw documents skills as being loadable from multiple locations (e.g., local project dir vs home dir), with precedence rules, and supports filtering based on environment/config/binary presence. Neuromancer keeps that flexibility but makes permissions explicit. ([Claude][7])

Agent Skills as a broader convention emphasizes progressive disclosure: metadata can be loaded without loading full instructions/resources until needed. Neuromancer adopts this to reduce prompt injection surface and token bloat. ([Claude][8])

---

## 5. v0.5 functional requirements mapping

### Required by you → implemented as

* **Minimum, extensible `core` for running an agent**
  `neuromancer-core` library crate: core traits (ToolBroker, MemoryStore, SecretsBroker, PolicyEngine). `neuromancer-agent` crate: rig-based agent execution runtime.
* **Detailed logging via OpenTelemetry**
  `tracing` + OTLP exporter; spans for every step; trace propagation across A2A and MCP.
* **Sub-agents can run skills, use MCP, and A2A**
  ToolBroker with 3 backends: SkillsExecutor, McpClientPool (via rig `.mcp_tool()`), A2aClient.
* **Centralized configuration format (TOML)**
  Single TOML file defines models, orchestrator/agent prompts, triggers, servers, secrets, and memory partitions.
* **Trigger system to orchestrator**
  Trigger Manager emits trigger events into System0 input queue; System0 processes turns and delegates as needed.
* **Triggers supported: Discord and cron**
  Discord via `twilight`; cron via `tokio-cron-scheduler`.
* **Secure secrets store with access controls**
  Secrets Broker with encrypted-at-rest store, ACLs, and "handle-based injection".
* **Hierarchical, partitioned memory**
  Memory partitions per agent/scope with explicit sharing rules.
* **Admin API for runtime introspection**
  Localhost HTTP endpoints via axum for task/agent/cron/memory visibility and manual task submission.

---

## 6. Core runtime design

### 6.1 Core interfaces (traits)

These traits define the modularity and testability surface of Neuromancer. They live in `neuromancer-core`.

```rust
// LlmProvider: REMOVED.
// Neuromancer does not define its own LLM abstraction. Use rig's
// `rig::completion::CompletionModel` and `rig::agent::Agent` directly.
// See §7 for rig integration details.

/// Policy-enforcing tool broker. Wraps rig tool definitions with
/// capability checks before execution.
trait ToolBroker: Send + Sync {
    /// List tools visible to this agent context, filtered by policy.
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec>;

    /// Execute a tool call after policy checks. Returns a rig-compatible ToolResult.
    async fn call_tool(&self, ctx: &AgentContext, call: ToolCall) -> Result<ToolResult>;
}

/// Persistent, partitioned memory store. See §13 for full specification.
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

/// Stacked policy gates evaluated before/after tool and LLM calls.
trait PolicyEngine: Send + Sync {
    async fn pre_tool_call(&self, ctx: &AgentContext, call: &ToolCall) -> PolicyDecision;
    async fn pre_llm_call(&self, ctx: &AgentContext, messages: &[ChatMessage]) -> PolicyDecision;
    async fn post_tool_call(&self, ctx: &AgentContext, result: &ToolResult) -> PolicyDecision;
}
```

### 6.2 Orchestrator loop

The orchestrator is the main event loop of `neuromancerd`. In v0.1-alpha it runs as a System0 `neuromancer-agent` instance, receives inbound turns through `orchestrator.turn`, and keeps a single persistent session history in the agent crate.

```
loop {
    select! {
        turn = input_message_queue.recv() => {
            system0_agent.execute_turn(global_session_id, turn.message).await?;
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
* Runtime executes one System0 turn against the single session.
* Delegated runs are tracked internally and exposed via:
  * `orchestrator.runs.list`
  * `orchestrator.runs.get`

The orchestrator prompt is loaded from `system_prompt_path` (or the XDG default), placeholders are rendered with runtime context (`{{AVAILABLE_AGENTS}}`, `{{AVAILABLE_TOOLS}}`, `{{ORCHESTRATOR_ID}}`), and that rendered content is supplied to `AgentRuntime` at instantiation time.

### 6.3 Agent execution state machine

Each sub-agent task execution follows a state machine. This replaces the linear 6-step loop from v0.5-r1 with a proper model that handles multi-step reasoning, checkpointing, and failure recovery.

```rust
enum TaskExecutionState {
    /// Agent runtime is being initialized: loading skills, connecting MCP, building rig Agent.
    Initializing { task: Task },

    /// rig Agent is reasoning: preparing the next chat() call.
    /// `iteration` counts Thinking→Acting cycles (capped by max_iterations).
    Thinking {
        conversation: ConversationContext,
        iteration: u32,
    },

    /// Agent has produced tool calls; executing them sequentially or in parallel.
    Acting {
        conversation: ConversationContext,
        pending_calls: Vec<ToolCall>,
        completed_calls: Vec<ToolResult>,
    },

    /// Agent needs external input (e.g., human clarification). Task is suspended
    /// with a checkpoint so it can resume when input arrives.
    WaitingForInput {
        conversation: ConversationContext,
        question: String,
        checkpoint: Checkpoint,
    },

    /// Agent has produced a candidate output. Self-assessment or verifier check.
    Evaluating {
        conversation: ConversationContext,
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
* Each **Thinking → Acting** cycle corresponds to one `agent.chat()` call in rig. The LLM response may contain tool calls (→ Acting) or a final answer (→ Evaluating or Completed).
* **Checkpointing** occurs at every state transition. The checkpoint includes the serialized `ConversationContext` and current state, persisted to SQLite. On crash recovery, the orchestrator can resume tasks from their last checkpoint.
* **WaitingForInput** is entered when the agent explicitly requests human clarification. The orchestrator sends the question to the trigger channel and suspends the task.
* **Evaluating** enables self-assessment: the agent (or a separate verifier model) reviews the candidate output. It may loop back to Thinking for refinement.
* **Conversation continuity across tasks:** When a task is related to a prior task, the new task's `ConversationContext` is initialized with a summary from the parent task (referenced via `parent_task_id`).

### 6.4 Conversation and message history

`ConversationContext` is the runtime message history container. It is **ephemeral** (lives for the duration of a task) and distinct from `MemoryStore` (persistent across tasks).

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
    redacted: bool,               // true if content was policy-filtered
}

struct ConversationContext {
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

**Message ordering in the context window:**

1. System prompt (agent identity, personality, constraints)
2. Memory context (relevant items from MemoryStore, injected as system messages)
3. Conversation history (accumulated ChatMessages from prior iterations)
4. Current user message / task instruction

**History accumulation during agent execution:**

Each Thinking → Acting cycle appends messages:
1. The LLM response (Assistant role, may contain tool calls)
2. Tool results (Tool role, one per tool call)
3. These become part of the conversation for the next `agent.chat()` call

**History vs. Memory distinction:**

| Aspect | ConversationContext | MemoryStore |
|---|---|---|
| Lifetime | Single task execution | Persistent across tasks |
| Scope | One agent, one task | Partitioned, shared per policy |
| Content | Full message history | Summaries, facts, artifacts |
| Token cost | Directly in context window | Injected selectively |

**Sub-agent context isolation:**

When the orchestrator dispatches a task to a sub-agent, the sub-agent receives a **fresh** `ConversationContext` containing:
1. The sub-agent's own system prompt
2. Relevant memory items for the sub-agent's authorized partitions
3. A context slice from the parent task (if any): the task instruction + summary of prior context, NOT the full parent conversation. This prevents token bloat and enforces need-to-know.

**rig integration:**

```rust
impl ConversationContext {
    /// Convert to rig's message format for agent.chat().
    fn to_rig_messages(&self) -> Vec<rig::completion::Message> { ... }

    /// Append a rig completion response back into the context.
    fn append_response(&mut self, response: &rig::completion::CompletionResponse) { ... }

    /// Check if truncation is needed and apply the configured strategy.
    async fn maybe_truncate(&mut self) -> Result<()> { ... }
}
```

### 6.5 Sub-agent remediation protocol

When a sub-agent encounters a problem, it sends a structured `SubAgentReport` to the orchestrator. The orchestrator evaluates the report and responds with a `RemediationAction`. This protocol replaces ad-hoc error handling with a well-defined contract.

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

**A2A state mapping:**

| SubAgentReport variant | A2A task state |
|---|---|
| Progress | `working` |
| InputRequired | `input_required` |
| Completed | `completed` |
| Failed | `failed` |
| Stuck | `failed` (with partial result) |
| ToolFailure | `working` (if retry eligible) or `failed` |
| PolicyDenied | `input_required` (escalation) or `failed` |

**Health monitoring:**

* **Heartbeat:** Sub-agents send `Progress` reports at configurable intervals (default: 30s). If no report arrives within `2 * heartbeat_interval`, the orchestrator marks the agent as unresponsive.
* **Watchdog:** Unresponsive agents are sent a cancellation signal. If they don't respond within `watchdog_timeout` (default: 60s), the orchestrator kills the runtime and marks the task as `Failed`.

**Circuit breaker:**

If an agent produces `N` failures within `M` minutes (configurable), the orchestrator stops dispatching new tasks to it and emits an alert. Default: 5 failures in 10 minutes.

```toml
[agents.browser.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"
circuit_breaker = { failures = 5, window = "10m" }
```

### 6.6 Task queue and lifecycle

The task queue is the central coordination point between the orchestrator and sub-agents.

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

enum TaskPriority {
    Low,
    Normal,
    High,
    Critical,
}

struct TaskOutput {
    artifacts: Vec<Artifact>,
    summary: String,
    token_usage: TokenUsage,
    duration: Duration,
}
```

**Queue implementation:**

* In-process: `tokio::sync::mpsc` channel for low-latency dispatch.
* Persistence: All tasks are written to SQLite on state transitions for crash recovery. On startup, the orchestrator reloads tasks in `Queued` or `Dispatched` state.
* Priority ordering: Tasks are dequeued by priority (Critical > High > Normal > Low), then by creation time within the same priority.

**Idempotency:**

Tasks with an `idempotency_key` are deduplicated. Before enqueuing, the orchestrator checks if a task with the same key exists in a non-terminal state (`Queued`, `Dispatched`, `Running`). If so, the new task is dropped and the existing task's ID is returned.

### 6.7 Error taxonomy

All Neuromancer errors map to a unified enum for consistent handling, logging, and reporting.

```rust
enum NeuromancerError {
    // Agent execution errors
    Agent(AgentError),

    // LLM provider errors (surfaced from rig)
    Llm(LlmError),

    // Tool execution errors
    Tool(ToolError),

    // Policy violations
    Policy(PolicyError),

    // Infrastructure errors
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
}

enum PolicyError {
    CapabilityDenied { agent_id: AgentId, capability: CapabilityRef },
    SecretAccessDenied { agent_id: AgentId, secret_ref: SecretRef },
    PartitionAccessDenied { agent_id: AgentId, partition: String },
    HeuristicBlocked { pattern: String, description: String },
}

enum InfraError {
    Database(sqlx::Error),
    Io(std::io::Error),
    Config(ConfigError),
    ContainerRuntime(String),
}
```

All errors implement `std::error::Error` and produce structured OTEL events with the error variant as an attribute.

---

## 7. LLM integration via Rig

### 7.1 Rig as first-class dependency

Rig is not an "optional adapter" — it is the **primary** LLM integration layer. Neuromancer does not define a custom `LlmProvider` trait. ([Docs.rs][9])

**Rationale:** Rig already provides:
* Multi-provider support (OpenAI, Anthropic, Gemini, etc.)
* `CompletionModel` trait for model abstraction
* `Agent` with tool use and conversation management
* MCP tool integration via `.mcp_tool()` (wraps rmcp)
* Embedding models and vector store abstractions for future RAG

Wrapping rig in a Neuromancer-specific trait would add indirection without value. Instead, Neuromancer uses rig types directly in `neuromancer-agent` and keeps `neuromancer-core` rig-free (traits only, no rig dependency).

**Dependency boundary:**

| Crate | rig dependency? | Reason |
|---|---|---|
| `neuromancer-core` | No | Defines traits only |
| `neuromancer-agent` | **Yes** (full) | Agent construction, execution, tool dispatch |
| `neuromancerd` | Transitive | Via `neuromancer-agent` |

### 7.2 Agent construction with rig AgentBuilder

Each sub-agent is constructed at dispatch time using rig's `AgentBuilder`:

```rust
use rig::agent::AgentBuilder;
use rig::providers::openai;

async fn build_agent(
    config: &AgentConfig,
    model_router: &ModelRouter,
    tools: Vec<Box<dyn rig::tool::Tool>>,
    mcp_tools: Vec<(McpToolDefinition, McpPeer)>,
    system_prompt: String,
) -> Result<rig::agent::Agent> {
    let model = model_router.resolve(&config.model_slot)?;

    let mut builder = model
        .agent(system_prompt);

    // Add policy-wrapped native tools
    for tool in tools {
        builder = builder.tool(tool);
    }

    // Add MCP tools via rig's mcp integration
    for (tool_def, peer) in mcp_tools {
        builder = builder.mcp_tool(tool_def, peer.into());
    }

    Ok(builder.build())
}
```

**Execution loop integration:**

Each Thinking → Acting cycle in the state machine (§6.3) corresponds to:

```rust
// In Thinking state:
let rig_messages = conversation.to_rig_messages();
let response = agent.chat(&current_instruction, rig_messages).await?;

// Parse response:
match response {
    // Contains tool calls → transition to Acting
    response if response.has_tool_calls() => {
        let tool_calls = response.extract_tool_calls();
        // transition to Acting { pending_calls: tool_calls, ... }
    }
    // Final answer → transition to Evaluating or Completed
    response => {
        let output = response.extract_text();
        // transition to Evaluating or Completed
    }
}
```

### 7.3 MCP tool integration via rig + rmcp

MCP tools are surfaced to rig agents via rig's built-in MCP support, which wraps `rmcp` tool definitions:

1. The `McpClientPool` (in `neuromancer-mcp`) maintains connections to configured MCP servers.
2. At agent construction time, allowed MCP tools are resolved from the pool.
3. Each MCP tool is wrapped via `builder.mcp_tool(tool_def, peer)`, making it callable through rig's standard tool dispatch.
4. The `ToolBroker` performs policy checks before tool execution, even for MCP tools.

### 7.4 Model router

The `ModelRouter` maps logical model slots to concrete rig `CompletionModel` instances:

```rust
struct ModelRouter {
    slots: HashMap<String, Box<dyn CompletionModel>>,
}

impl ModelRouter {
    /// Resolve a slot name (e.g., "planner", "executor") to a rig CompletionModel.
    fn resolve(&self, slot: &str) -> Result<&dyn CompletionModel>;
}
```

Slot assignments are driven by:
1. Central TOML config defaults (`[models]` section)
2. Per-agent overrides (`[agents.<name>.models]`)
3. Per-skill hinting in frontmatter (`metadata.neuromancer.models.preferred`)

Example config:

```toml
[models]
orchestrator = { provider = "groq", model = "openai/gpt-oss-120B" }
planner = { provider = "groq", model = "openai/gpt-oss-120B" }
executor = { provider = "groq", model = "openai/gpt-oss-120B" }
browser = { provider = "groq", model = "openai/gpt-oss-120B" }
verifier = { provider = "groq", model = "openai/gpt-oss-120B" }
```

### 7.5 Embeddings and RAG (future)

Rig provides `EmbeddingModel` and `VectorStoreIndex` abstractions. ([Docs.rs][9])

When Neuromancer adds semantic memory search (post-v0.5), it will use rig's embedding pipeline:
* `model.embed_document()` for storing memory items with embeddings
* `vector_store.top_n()` for semantic retrieval during memory context injection

This is a non-goal for v0.5 but the architecture accommodates it without breaking changes.

---

## 8. Skills system (still critical, now permissioned)

### 8.1 Skill format

Neuromancer remains compatible with the emerging **Agent Skills** pattern:

* `SKILL.md` with frontmatter + instructions
* resources in the skill directory
* progressive loading (metadata first; instructions/resources when invoked) ([Claude][8])

Neuromancer also preserves useful OpenClaw concepts like multiple skill directories and filtering, but makes the permission model explicit and enforceable at runtime. ([Claude][7])

### 8.2 Neuromancer permissions in frontmatter (extension)

Add an optional section under `metadata.neuromancer`:

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
    execution:
      mode: "isolated"  # isolated | host
      network: "egress-web"  # policy name
    models:
      preferred: "browser_model"
    safeguards:
      human_approval:
        - "purchase"
        - "file_write_outside_workspace"
---
```

**Important:** skill metadata is not sufficient to grant permissions. It's a *declaration*. The central config must authorize it.

### 8.3 Parsing frontmatter (Rust crate reality)

Many skill ecosystems use YAML frontmatter. But `serde_yaml` is marked "no longer maintained" in its docs. ([Docs.rs][10])

For v0.5, implement YAML parsing using maintained components:

* `yaml-rust2` as a YAML 1.2 implementation ([Docs.rs][11])
* optionally `serde_yaml2` for serde integration (if you want typed structs)
* `gray_matter` as a convenient frontmatter extractor (supports YAML/TOML/JSON) ([Docs.rs][12])

### 8.4 Skill execution modes

Each skill can be executed as:

* **Host execution**: only when explicitly allowed (dangerous by default)
* **Isolated execution**: run in a sandbox (container) with minimal mounts and no ambient secrets

This directly responds to the demonstrated "skills as supply chain" risk, where provenance + mediated execution + specific permissions are repeatedly cited as the missing layer. ([1Password][4])

---

## 9. MCP integration (tools/services)

### 9.1 SDK choice

Use **RMCP** (`rmcp`) from the official MCP Rust SDK repository; it's explicitly described as the official Rust SDK and supports Tokio. ([GitHub][5])

MCP tools are surfaced to agents via **rig's `.mcp_tool()` integration**, which wraps rmcp tool definitions as rig-compatible tool implementations. The `neuromancer-mcp` crate manages connections and lifecycle; the `neuromancer-agent` crate passes them to rig.

v0.5 features:

* Spawn stdio MCP servers (Node, Python, Go, etc.) as child processes
* Connect to remote MCP servers
* Maintain a registry of server configs and allowed tools per agent

### 9.2 MCP server lifecycle manager

Responsibilities:

* Start/stop servers on demand
* Health checks
* Restart policies
* Per-server environment injection (from secrets broker)
* Per-agent allowlist filtering: agent sees only tools from allowed servers (and optionally tool-level allowlists)

---

## 10. A2A integration (agent-to-agent)

### 10.1 Binding

Implement **A2A HTTP+JSON/REST** endpoints first (v0.5), because it maps cleanly to axum:

* `POST /message:send`
* `POST /message:stream` (SSE)
* `GET /tasks/{id}`
* `GET /tasks`
* `POST /tasks/{id}:cancel`
* `POST /tasks/{id}:subscribe` (SSE) ([A2A Protocol][6])

Requests use `Content-Type: application/a2a+json`. ([A2A Protocol][6])

Expose an Agent Card at:

* `/.well-known/agent-card.json` ([A2A Protocol][6])

### 10.2 Permission model over A2A

Every A2A request is evaluated by policy:

* peer identity (mTLS cert subject / JWT claims)
* requested operation
* requested resource (memory partition, tool invocation, etc.)

A2A spec guidance around permissions and non-leaky error behavior is adopted (e.g., don't reveal resource existence to unauthorized clients). ([A2A Protocol][6])

### 10.3 "Out-of-bounds permission request" workflow

This workflow is now formalized via the remediation protocol (§6.5):

1. Sub-agent fails policy check → emits `SubAgentReport::PolicyDenied`
2. Orchestrator evaluates: if `GrantTemporary` is appropriate (configured), issues a scoped grant
3. Otherwise, `EscalateToUser`: sends question via trigger channel (Discord DM or configured approver)
4. If user approves: issues a **one-time grant** scoped to `task_id`
5. If user denies: `Abort` the task with a clear reason

---

## 11. Central TOML configuration (org-tree)

Use a single TOML file as the source of truth.

For v0.1-alpha bootstrap:
* Run `neuroctl install` (or `neuroctl install --config <path>`) to bootstrap missing config/runtime directories and default `SYSTEM.md` prompt files (never overwriting existing files). Use `--override-config` to overwrite the target config from bootstrap defaults.
* XDG roots resolve under `$XDG_CONFIG_HOME/neuromancer` (or `~/.config/neuromancer`) and `$XDG_DATA_HOME/neuromancer` (or `~/.local/neuromancer`).
* Installer bootstraps by copying `defaults/bootstrap/` into the XDG config root (non-overwriting). Per-agent prompt files are created at `agents/<agent_name>/SYSTEM.md` from `defaults/templates/agent/SYSTEM.md` only for configured agents.
* Bootstrap config is intentionally agent-empty; per-agent `agents/<agent_name>/SYSTEM.md` files are created only for configured agents.
* Daemon startup fails if configured/default prompt files are missing or empty.

### 11.1 Design principles

* **Explicit capabilities**, no ambient permissions
* **Hierarchical inheritance** (org-tree):

  * children inherit parent defaults
  * can only reduce permissions unless explicitly allowed to expand (config flag)
* Every capability has:

  * a stable ID
  * audit metadata (owner, purpose)
  * scope (agent, skill, task)

### 11.2 Example config

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

[memory]
backend = "sqlite"
sqlite_path = "/var/lib/neuromancer/data/memory.db"

[models]
planner = { provider = "openai", model = "gpt-4o" }
executor = { provider = "openai", model = "gpt-4o" }
browser = { provider = "anthropic", model = "claude-sonnet-4-5-20250929" }
verifier = { provider = "openai", model = "gpt-4o-mini" }

[mcp_servers.playwright]
kind = "child_process"
command = ["npx", "-y", "@playwright/mcp-server"]
sandbox = "container"

[mcp_servers.filesystem]
kind = "builtin"
sandbox = "host"
allowed_roots = ["/var/lib/neuromancer/workspaces"]

[a2a]
bind_addr = "0.0.0.0:8800"
agent_card_signing = "optional"

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"
max_iterations = 30

# --- Agents (sub-agents only, no "core" agent) ---

[agents.planner]
mode = "inproc"
models.planner = "planner"
models.executor = "executor"
system_prompt_path = "prompts/agents/planner/SYSTEM.md"
capabilities.skills = ["task_manager", "code_review"]
capabilities.mcp_servers = []
capabilities.a2a_peers = ["browser", "files"]
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:planner"]

[agents.planner.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"

[agents.browser]
mode = "container"
image = "neuromancer:0.5"
models.executor = "browser"
system_prompt_path = "prompts/agents/browser/SYSTEM.md"
capabilities.skills = ["browser_summarize", "browser_clickpath"]
capabilities.mcp_servers = ["playwright"]
capabilities.secrets = ["web_login_cookiejar"]
capabilities.memory_partitions = ["workspace:default", "agent:browser"]

[agents.browser.health]
heartbeat_interval = "30s"
watchdog_timeout = "60s"
circuit_breaker = { failures = 5, window = "10m" }

[agents.files]
mode = "container"
image = "neuromancer:0.5"
system_prompt_path = "prompts/agents/files/SYSTEM.md"
capabilities.skills = ["fs_readwrite"]
capabilities.mcp_servers = ["filesystem"]
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:files"]

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
agent = "github-summarizer"
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
```

### 11.3 Hot reload

Add a file watcher to reload config safely (with validation + diff). `notify` provides cross-platform filesystem notifications. ([Docs.rs][13])

On reload:
* Validate the new config fully before applying
* Diff against current config to determine what changed
* Agent capability changes take effect on next task dispatch (not mid-task)
* Routing rule changes take effect immediately
* Trigger config changes restart the affected trigger source

---

## 12. Secrets store and access controls

### 12.1 Core rule

**Secrets are not LLM context.**
Agents reference secrets by **handle**; only tool execution contexts receive secret values (ideally injected into a subprocess/container environment).

This is a direct architectural countermeasure to the class of "keys serialized into prompt context" bugs. ([GitHub][1])

### 12.2 Implementation (v0.5)

* **Data store**: SQLite (via `sqlx`) for metadata + ciphertext blobs
  SQLx is async and supports SQLite/Postgres/MySQL with compile-time checked queries. ([GitHub][14])
* **Encryption at rest**: `age` for encrypting secret payloads (simple, modern, secure). ([Docs.rs][15])
* **Master key storage**: OS keychain via `keyring` (platform stores supported behind feature flags). ([Docs.rs][16])
* **In-memory handling**: `secrecy::SecretBox/SecretString` + `zeroize` to reduce accidental exposure and prevent accidental serde serialization. ([Docs.rs][17])

### 12.3 Secret ACL model

Each secret has:

* allowed agents
* allowed skills (optional)
* allowed MCP servers/tools (optional)
* allowed injection modes (env var, header, file mount)
* TTL / rotation metadata

### 12.4 Auditing

Every secret access event emits:

* `secret_id`, `agent_id`, `task_id`, `tool_id`, `reason`
* outcome (granted/denied)
* hashed callsite metadata

---

## 13. Hierarchical, partitioned memory

### 13.1 Partitioning model

Memory is divided into partitions with independent policies:

* `task:<task_id>` ephemeral scratch (expires)
* `agent:<agent_id>` private long-term
* `workspace:<name>` shared project memory
* `org:<name>` shared across workspaces (optional)
* `global` system metadata (tools catalog snapshots, etc.)

Each agent is allowed to read/write only its authorized partitions.

### 13.2 Storage choice (v0.5)

* SQLite via `sqlx` (simple ops + easy backup + transactional) ([GitHub][14])
* Optional vector embeddings later:

  * via Rig's embeddings abstractions ([Docs.rs][9])
  * or external vector DB as MCP service

### 13.3 Memory record schema (conceptual)

* `memory_id` (returned as `MemoryId` from `put()`)
* `partition`
* `kind`: {message, summary, fact, artifact, link, tool_result}
* `content` (text or structured JSON)
* `tags`
* `created_at`, `expires_at`
* `source`: {discord, cron, a2a, tool}
* `sensitivity`: {public, private, secret_ref_only}
* optional `embedding_ref`

### 13.4 "Never store raw secrets" rule

Memory may store:

* secret handles and references
* redacted snippets
  But never plaintext secrets (enforced by policy + linter + runtime filters).

### 13.5 Memory usage during agent execution

This section specifies **when** and **what** agents read from and write to the MemoryStore during task execution.

**On task initialization (read):**

1. Query `workspace:<name>` partition for items matching the task's topic/tags (relevance ranking by recency + tag overlap).
2. Query `agent:<agent_id>` partition for the agent's own long-term context.
3. Inject retrieved items into the `ConversationContext` as system messages, respecting the token budget. Items are ordered: most relevant first, truncated if budget is exceeded.

**After each tool call (write):**

1. Store the tool result in `task:<task_id>` partition with `kind: tool_result`.
2. If the tool produced a notable artifact (file, URL, data), store it with `kind: artifact`.

**On task completion (write):**

1. Generate a summary of the task execution (via the agent's final output or a dedicated summarizer).
2. Store the summary in `agent:<agent_id>` partition with `kind: summary`.
3. Extract and store factual information (e.g., "the user prefers X", "repo Y uses framework Z") in the appropriate partition with `kind: fact`.

**Explicit agent tools:**

Agents have access to two memory-related tools:
* **`remember`**: Explicitly store a fact or note. The agent calls this when it determines something is worth persisting. Writes to `agent:<agent_id>` or `workspace:<name>` per policy.
* **`recall`**: Search memory by query string. Returns matching items from authorized partitions. The agent calls this during reasoning when it needs historical context beyond what was injected at init.

### 13.6 Memory crate separation

The `MemoryStore` trait is defined in `neuromancer-core` (no implementation).

The SQLite implementation lives in a separate crate: `neuromancer-memory-simple/`.

```
neuromancer-core/src/memory.rs       # trait MemoryStore, MemoryItem, MemoryQuery, etc.
neuromancer-memory-simple/src/lib.rs  # SqliteMemoryStore: impl MemoryStore
neuromancer-memory-simple/migrations/ # SQLite schema migrations
```

This separation allows:
* `neuromancer-core` to remain lightweight with no sqlx dependency
* Future alternative implementations (e.g., `neuromancer-memory-pg` for Postgres, `neuromancer-memory-vector` for semantic search)
* The orchestrator to depend on `neuromancer-core` without pulling in storage dependencies

---

## 14. Trigger system (Discord + cron)

### 14.1 Trigger manager contract

A trigger source produces:

```rust
struct TriggerEvent {
    trigger_id: String,
    occurred_at: DateTime<Utc>,
    principal: Principal,         // discord user, system cron, etc.
    payload: TriggerPayload,      // message text, attachments, schedule context, etc.
    metadata: TriggerMetadata,    // channel_id, guild_id, thread_id, etc.
}
```

The Trigger Manager owns a `tokio::sync::mpsc::Sender<TriggerEvent>` and sends events to the orchestrator's main loop.

### 14.2 Discord trigger

The Discord trigger provides a full specification of how Discord messages become tasks and how responses are delivered.

**Crate choice:** `twilight` — a modular, scalable Discord library that separates gateway, HTTP, and model concerns. ([Docs.rs][18])

#### 14.2.1 Configuration

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

enum ThreadMode {
    /// Create a new Discord thread for each task. Responses go to the thread.
    PerTask,
    /// Use the existing thread if the message is in one; otherwise create.
    Inherit,
    /// No thread management; respond in the same channel.
    Disabled,
}

enum ActivationRule {
    /// Process all messages in this channel.
    All,
    /// Only process messages that @mention the bot.
    Mention,
    /// Only process messages starting with a prefix (e.g., "!nm").
    Prefix(String),
    /// Only process messages that are replies to the bot.
    Reply,
}

enum DmPolicy {
    /// Accept DMs from any user.
    Open,
    /// Accept DMs only from users in allowed_guilds.
    AllowedUsersOnly,
    /// Reject all DMs.
    Disabled,
}
```

#### 14.2.2 Message-to-task mapping

When a Discord message passes activation and rate limit checks:

1. Extract message content, attachments, and thread context.
2. If attachments are present: download them to a task-scoped workspace directory. Include file paths in the task instruction.
3. Apply `channel_routes` policy checks for activation/rate limiting before emitting a trigger event.
4. If `thread_mode` is `PerTask`: create a Discord thread for the task. All subsequent responses for this task go to the thread.
5. Construct a `TriggerEvent` with `principal` set to the Discord user, `payload` containing the message text + attachment paths, and `metadata` containing channel/guild/thread IDs.
6. Send to the orchestrator.

#### 14.2.3 Response delivery

When a task produces output:

1. Format the response according to `DiscordResponseConfig`.
2. If the response exceeds `max_message_length`:
   * `Split`: break into multiple messages at paragraph/code-block boundaries.
   * `FileAttachment`: send as a `.md` file attachment with a brief summary message.
   * `Truncate`: truncate with a "... (truncated)" suffix.
3. If `use_embeds` is true and the output contains structured data (tables, key-value pairs), format as Discord embeds.
4. If `auto_code_blocks` is true, wrap code snippets in Discord code blocks with language hints.
5. Send to the appropriate channel/thread.

```rust
struct DiscordResponseConfig {
    max_message_length: usize,   // default: 2000 (Discord limit)
    overflow: OverflowStrategy,
    use_embeds: bool,
    auto_code_blocks: bool,
}

enum OverflowStrategy {
    Split,
    FileAttachment,
    Truncate,
}
```

#### 14.2.4 Rate limiting

```rust
struct RateLimitConfig {
    per_user_per_minute: u32,      // default: 10
    per_channel_per_minute: u32,   // default: 30
    max_concurrent_tasks: u32,     // default: 5
}
```

Rate limits are tracked in-memory with a sliding window. When exceeded, the bot reacts with a rate-limit emoji and does not create a task.

#### 14.2.5 Connection lifecycle

* On startup: connect to Discord gateway via twilight, register for `MESSAGE_CREATE` events.
* On disconnect: automatic reconnection with exponential backoff (handled by twilight's gateway).
* On shutdown: gracefully close the gateway connection. In-flight tasks continue; new events are dropped.

### 14.3 Cron trigger

Cron triggers produce tasks on a schedule, with structured task templates replacing raw payload strings.

**Crate:** `tokio-cron-scheduler` for cron-like scheduling on Tokio. ([Docs.rs][20])

#### 14.3.1 Configuration

Each cron trigger is a named job with a schedule, a task template, execution parameters, and notification rules. See §11.2 for the full TOML format.

Key fields:

| Field | Description |
|---|---|
| `id` | Unique job identifier, used for idempotency and admin API |
| `description` | Human-readable purpose |
| `schedule` | Cron expression (6-field: sec min hour day month weekday) |
| `task_template.agent` | Target agent for the generated task |
| `task_template.instruction` | Instruction text, supports `{{variable}}` template syntax |
| `task_template.parameters` | Key-value pairs injected into the instruction template |
| `execution.timeout` | Max execution time before the task is cancelled |
| `execution.idempotency_key` | Template for dedup key (e.g., `"job-{{date}}"`) |
| `execution.dedup_window` | Time window for idempotency dedup |
| `execution.on_failure` | `"retry"` or `"skip"` |
| `execution.max_retries` | Max retry attempts |
| `notification.on_success` | Channel + template for success notification |
| `notification.on_failure` | Channel + template for failure notification |

#### 14.3.2 Template rendering

Instruction templates use `minijinja` for rendering. Available variables:

* All keys from `task_template.parameters`
* `{{date}}` — current date (YYYY-MM-DD)
* `{{datetime}}` — current datetime (ISO 8601)
* `{{job_id}}` — the cron job ID

#### 14.3.3 Execution flow

1. `tokio-cron-scheduler` fires at the scheduled time.
2. Render the instruction template with current parameters.
3. Compute the `idempotency_key` (if configured).
4. Check dedup: if a task with the same idempotency key exists in a non-terminal state within `dedup_window`, skip.
5. Create a `Task` with the rendered instruction, assigned agent, and priority `Normal`.
6. Enqueue via the orchestrator.
7. On completion/failure: render and send notification (if configured).

#### 14.3.4 Management via Admin API

Cron jobs can be managed at runtime through the Admin API (§21):
* `GET /admin/cron` — list all cron jobs with next-fire times
* `POST /admin/cron/{id}/trigger` — manually trigger a cron job now
* `POST /admin/cron/{id}/disable` — disable without removing
* `POST /admin/cron/{id}/enable` — re-enable

---

## 15. Observability via OpenTelemetry

### 15.1 Why OTLP

`opentelemetry-otlp` supports exporting telemetry data (logs/metrics/traces) to an OpenTelemetry Collector and common backends, via gRPC or HTTP. ([Docs.rs][21])

### 15.2 What we instrument (required)

Each task run emits spans:

* `trigger.receive`
* `task.create`
* `orchestrator.turn`
* `agent.dispatch`
* `agent.state_transition` (from → to state, with state machine context)
* `llm.call` (model id, token counts, latency)
* `tool.call` (tool id, MCP server, A2A peer, skill id)
* `secret.access` (handle id only, no value)
* `memory.query` / `memory.put`
* `policy.decision` (allow/deny + reason code)
* `remediation.action` (what the orchestrator decided)

### 15.3 Log redaction requirements

* Never log raw prompts containing secrets
* Redact configured patterns
* Emit "structured events" rather than dumping payloads

(These are must-haves to prevent the ecosystem-wide "keys in logs" class of failures.)

---

## 16. Component isolation and container orchestration

### 16.1 Container supervisor

Use `bollard` as the Docker API client; it supports API version negotiation and is built from the published Docker schema. ([Docs.rs][22])

v0.5 implements:

* start/stop agent containers
* mount workspace directories read-only or read-write per agent policy
* per-agent network policy presets (basic)
* resource limits (memory/cpu) per container

### 16.2 Isolation defaults

* Sub-agents run in containers by default
* Minimal filesystem mounts
* Secrets injected only into the specific tool execution context
* No Docker socket exposure to agents unless explicitly granted

---

## 17. Security model (the "missing trust layer")

This is the heart of the design.

### 17.1 Identity + least privilege

* Every agent has an identity (agent_id + credentials for A2A)
* Every external capability is explicit and revocable
* This directly matches the industry recommendation that permissions must be "specific, revocable, continuously enforced," not granted once and forgotten. ([1Password][4])

### 17.2 Policy engine + verification middleware

Neuromancer supports stacked gates:

1. **Static gates** (config policy): allow/deny by capability.
2. **Heuristic gates** (cheap): detect dangerous patterns (rm -rf, curl | sh, etc.).
3. **Verifier gate** (optional): call a dedicated "safeguard model" for tool call approval.
4. **Human-in-the-loop**: for destructive/sensitive actions (via remediation protocol §6.5).

### 17.3 Skill linting (v0.5 foundation)

Given research showing that skills often instruct agents to pass secrets into prompts/logs ([Snyk][2]), Neuromancer adds:

* **Skill linter** at install/load time:

  * flags "print your API key" patterns
  * flags instructions that tell the agent to store secrets in memory/config
  * flags commands that download/execute remote scripts
* **Runtime outbound filter** (optional):

  * prevent echoing secret handles resolved into plaintext

---

## 18. Crate selection (researched shortlist)

### Daemon/runtime fundamentals

* **Async runtime:** `tokio` (ecosystem standard; required by rmcp and rig) ([GitHub][5])
* **HTTP server (A2A/admin):** `axum` (ergonomics + modularity; uses tower ecosystem). ([Docs.rs][23])
* **TLS:** `rustls` (secure defaults, no unsafe features by default). ([Docs.rs][24])

### LLM integration

* **LLM framework:** `rig-core` (**mandatory** — primary LLM integration, agent construction, tool dispatch) ([Docs.rs][9])

### Protocols

* **MCP:** `rmcp` official SDK, surfaced through rig's `.mcp_tool()` ([GitHub][5])
* **A2A:** implement HTTP+JSON binding in axum using the spec endpoints ([A2A Protocol][6])

### Observability

* **OTLP exporter:** `opentelemetry-otlp` (OTLP over gRPC/HTTP). ([Docs.rs][21])

### Storage

* **DB:** `sqlx` (async SQL toolkit, supports SQLite/Postgres/MySQL). ([GitHub][14])

### Secrets

* **In-memory secret hygiene:** `secrecy` + `zeroize` ([Docs.rs][17])
* **OS keychain:** `keyring` ([Docs.rs][16])
* **At-rest encryption:** `age` ([Docs.rs][15])

### Triggers

* **Discord:** `twilight` ([Docs.rs][18])
* **Cron:** `tokio-cron-scheduler` ([Docs.rs][20])

### Skills parsing

* **Frontmatter extraction:** `gray_matter` ([Docs.rs][12])
* **YAML parsing:** `yaml-rust2` (serde_yaml is unmaintained) ([Docs.rs][11])

### Template rendering

* **Templates:** `minijinja` (lightweight Jinja2-compatible engine for cron task templates) ([Docs.rs][25])

### Container orchestration

* **Docker:** `bollard` ([Docs.rs][22])

---

## 19. Crate layout and milestones

### 19.1 Crate layout

```
neuromancer/
  Cargo.toml                      # workspace root
  neuromancer-core/               # Core traits: ToolBroker, MemoryStore, SecretsBroker,
                                  #   PolicyEngine, error types, shared types.
                                  #   NO rig dependency. NO sqlx dependency.
  neuromancer-agent/              # Agent execution runtime: state machine, rig Agent
                                  #   construction, ConversationContext, model router.
                                  #   Depends on: neuromancer-core, rig-core.
  neuromancerd/                   # Daemon binary: config loading, signal handling,
                                  #   lifecycle management, admin API server.
                                  #   Depends on: all crates.
  neuromancer-a2a/                # A2A HTTP+JSON endpoints (axum handlers),
                                  #   Agent Card serving, task state mapping.
  neuromancer-mcp/                # MCP client pool: wraps rmcp, manages server
                                  #   lifecycle, health checks.
  neuromancer-skills/             # Skill loading, frontmatter parsing, linting,
                                  #   permission declaration extraction.
  neuromancer-triggers/           # Trigger manager, Discord trigger (twilight),
                                  #   cron trigger (tokio-cron-scheduler).
  neuromancer-memory-simple/      # SQLite MemoryStore implementation.
                                  #   Depends on: neuromancer-core, sqlx.
  neuromancer-secrets/            # Secrets broker: encrypted store, ACL enforcement,
                                  #   handle resolution, injection.
```

### 19.2 Implementation milestones

1. **Repo + crate layout + workspace Cargo.toml**
2. **`neuromancer-core`**: traits, error types, shared types
3. **TOML config + validation + hot reload** (in `neuromancerd`)
4. **OTEL tracing pipeline**
5. **`neuromancer-secrets`**: keyring master key, age-encrypted payloads, ACL enforcement
6. **`neuromancer-memory-simple`**: partitioned SQLite schema, basic query
7. **`neuromancer-mcp`**: MCP client pool using rmcp
8. **`neuromancer-agent`**: rig-based agent runtime, state machine, conversation context
9. **`neuromancer-a2a`**: A2A HTTP+JSON endpoints
10. **`neuromancer-triggers`**: cron + Discord
11. **`neuromancer-skills`**: skill loader + permission declarations + linter MVP
12. **`neuromancerd`**: daemon lifecycle, admin API, integration

---

## 20. Daemon lifecycle

### 20.1 Startup sequence

1. Load and validate TOML config.
2. Initialize OTEL tracing pipeline.
3. Open SQLite databases (memory, secrets, task queue).
4. Initialize Secrets Broker (unlock master key from OS keychain).
5. Initialize MCP Client Pool (start configured MCP servers).
6. Build Agent Registry (validate agent configs, pre-resolve model slots).
7. Start Trigger Manager (connect to Discord gateway, schedule cron jobs).
8. Recover in-flight tasks: reload tasks in `Queued`/`Dispatched`/`Running` states from SQLite, re-enqueue or resume from checkpoints.
9. Start Admin API HTTP server.
10. Enter Orchestrator main loop.

### 20.2 Signal handling

| Signal | Behavior |
|---|---|
| `SIGTERM` | Graceful shutdown: stop accepting new triggers, drain in-flight tasks (with timeout), persist checkpoints, exit. |
| `SIGINT` | Same as SIGTERM (for interactive use). |
| `SIGHUP` | Hot reload: re-read TOML config, apply changes per §11.3 rules. |

### 20.3 Graceful shutdown sequence

1. Stop Trigger Manager (disconnect Discord, stop cron scheduler).
2. Stop accepting new tasks in the queue.
3. Wait for in-flight tasks to complete (up to `shutdown_timeout`, default: 30s).
4. For tasks that don't complete in time:
   * Persist their current checkpoint to SQLite.
   * Mark them as `Suspended` with reason "daemon shutdown".
5. Close MCP server connections (send graceful shutdown to child processes).
6. Stop container-based agents (send SIGTERM to containers, wait, then SIGKILL).
7. Flush OTEL spans.
8. Close SQLite databases.
9. Exit with code 0.

### 20.4 Restart recovery

On next startup, the daemon:
1. Finds tasks in `Suspended` state with reason "daemon shutdown".
2. For tasks with valid checkpoints: resumes from the checkpoint (restores `ConversationContext` and state machine state).
3. For tasks without checkpoints or with corrupted checkpoints: marks as `Failed` with error `CheckpointCorrupted`.

---

## 21. Admin API

A localhost-only HTTP API for runtime introspection and management. Served by axum on a configurable port (default: `127.0.0.1:9090`).

### 21.1 Task endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/tasks` | List tasks with optional filters (state, agent, time range) |
| `GET` | `/admin/tasks/{id}` | Get task details including state, checkpoints, output |
| `POST` | `/admin/tasks` | Submit a manual task (instruction + target agent) |
| `POST` | `/admin/tasks/{id}/cancel` | Cancel a running task |

### 21.2 Agent endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/agents` | List agents with health status and current task |
| `GET` | `/admin/agents/{id}` | Agent details: config, health, recent tasks, circuit breaker state |

### 21.3 Cron endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/cron` | List cron jobs with schedule, next fire time, last result |
| `POST` | `/admin/cron/{id}/trigger` | Manually trigger a cron job now |
| `POST` | `/admin/cron/{id}/disable` | Disable a cron job |
| `POST` | `/admin/cron/{id}/enable` | Enable a cron job |

### 21.4 Memory and system endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/memory/stats` | Memory store stats: partition sizes, item counts, GC status |
| `GET` | `/admin/health` | Daemon health: uptime, task counts, agent states, MCP server status |
| `POST` | `/admin/config/reload` | Trigger config hot-reload (same as SIGHUP) |

### 21.5 Security

* Bind to `127.0.0.1` only (no remote access by default).
* Optional bearer token authentication for environments where localhost access is shared.
* All admin actions are logged to OTEL with `admin.action` spans.

---

## 22. Open questions (intentionally deferred)

* How strongly do you want to enforce skill provenance in v0.5 (signatures, SBOM, pinned git SHAs)?
* Should "file tools" be a built-in tool surface (preferred for safety) or only via MCP servers (preferred for protocol purity)?
  (My bias: built-in restricted FS tool for v0.5, plus MCP support for external servers.)
* Should the Admin API support WebSocket subscriptions for real-time task/agent status updates, or is polling sufficient for v0.5?
* What is the maximum conversation history size before mandatory truncation? (Current proposal: per-agent configurable, default 128k tokens.)
* Should cron job definitions be splittable across multiple TOML files (e.g., `cron.d/` directory pattern), or must everything remain in the single config file for v0.5?

---

[1]: https://github.com/openclaw/openclaw/issues/11202 "https://github.com/openclaw/openclaw/issues/11202"
[2]: https://snyk.io/blog/openclaw-skills-credential-leaks-research/ "https://snyk.io/blog/openclaw-skills-credential-leaks-research/"
[3]: https://github.com/openclaw/openclaw/issues/10864 "https://github.com/openclaw/openclaw/issues/10864"
[4]: https://1password.com/blog/from-magic-to-malware-how-openclaws-agent-skills-become-an-attack-surface "https://1password.com/blog/from-magic-to-malware-how-openclaws-agent-skills-become-an-attack-surface"
[5]: https://github.com/modelcontextprotocol/rust-sdk "https://github.com/modelcontextprotocol/rust-sdk"
[6]: https://a2a-protocol.org/latest/specification/ "https://a2a-protocol.org/latest/specification/"
[7]: https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices "https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices"
[8]: https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview "https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview"
[9]: https://docs.rs/rig-core "https://docs.rs/rig-core"
[10]: https://docs.rs/serde_yaml/ "https://docs.rs/serde_yaml/"
[11]: https://docs.rs/yaml-rust2/latest/yaml_rust2/ "https://docs.rs/yaml-rust2/latest/yaml_rust2/"
[12]: https://docs.rs/gray_matter "https://docs.rs/gray_matter"
[13]: https://docs.rs/notify/latest/notify/trait.Watcher.html "https://docs.rs/notify/latest/notify/trait.Watcher.html"
[14]: https://github.com/launchbadge/sqlx "https://github.com/launchbadge/sqlx"
[15]: https://docs.rs/crate/age/latest "https://docs.rs/crate/age/latest"
[16]: https://docs.rs/keyring "https://docs.rs/keyring"
[17]: https://docs.rs/secrecy "https://docs.rs/secrecy"
[18]: https://docs.rs/twilight/latest/x86_64-pc-windows-msvc/twilight/ "https://docs.rs/twilight/latest/x86_64-pc-windows-msvc/twilight/"
[19]: https://docs.rs/serenity/latest/serenity/ "https://docs.rs/serenity/latest/serenity/"
[20]: https://docs.rs/crate/tokio-cron-scheduler/latest "https://docs.rs/crate/tokio-cron-scheduler/latest"
[21]: https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/ "https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/"
[22]: https://docs.rs/bollard "https://docs.rs/bollard"
[23]: https://docs.rs/axum/latest/axum/ "https://docs.rs/axum/latest/axum/"
[24]: https://docs.rs/rustls "https://docs.rs/rustls"
[25]: https://docs.rs/minijinja "https://docs.rs/minijinja"
