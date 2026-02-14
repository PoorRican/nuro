# neuromancerd

Daemon binary hosting the System0 orchestrator runtime. Loads config, starts the admin API (Axum HTTP), processes user turns via a message queue, delegates to sub-agent runtimes, and journals all thread events to disk.

System0 is itself an `AgentRuntime` (from `neuromancer-agent`) whose `ToolBroker` can spin up and delegate to other `AgentRuntime` instances. This crate is the integration layer that wires everything together.

## Build & Test

```bash
cargo build -p neuromancerd
cargo test -p neuromancerd
cargo run -p neuromancerd -- -c neuromancer.toml
cargo run -p neuromancerd -- --validate -c neuromancer.toml
```

## Module Map

| File | Purpose |
|------|---------|
| `main.rs` | CLI args (clap), boot sequence, config watcher, admin server spawn, shutdown loop |
| `config.rs` | TOML loading, validation (model slots, agent refs, MCP servers, prompt files), hot-reload via `notify` file watcher |
| `admin.rs` | Axum HTTP routes: `POST /rpc` (JSON-RPC 2.0), `GET /admin/health`, `POST /admin/config/reload` |
| `message_runtime.rs` | **Core module** (~3.5K lines): `MessageRuntime`, `RuntimeCore`, `System0ToolBroker`, `SkillToolBroker`, `ThreadJournal`, LLM clients |
| `shutdown.rs` | Unix signal handler: SIGTERM/SIGINT -> shutdown, SIGHUP -> config reload |
| `telemetry.rs` | OTEL tracing init with optional OTLP exporter, JSON stdout layer |

## Turn Processing Flow

```
neuroctl orchestrator turn "message"
  -> POST /rpc (admin.rs) -> orchestrator.turn
    -> MessageRuntime::orchestrator_turn() -> sends TurnRequest to mpsc channel
      -> turn_worker receives -> RuntimeCore::process_turn()
        -> journal message_user event
        -> orchestrator AgentRuntime::execute_turn() (Think->Act loop)
          -> System0ToolBroker.call_tool() when LLM requests tools
            -> delegate_to_agent: sub-agent AgentRuntime::execute(task)
        -> journal message_assistant + tool events
      <- OrchestratorTurnResult { turn_id, response, delegated_runs, tool_invocations }
```

## Key Components in message_runtime.rs

**MessageRuntime** -- Public API surface. Owns the turn queue (`mpsc::Sender<TurnRequest>`). Exposes all `orchestrator_*` methods called from admin.rs RPC dispatch.

**RuntimeCore** -- Internal state that lives inside the `turn_worker` task. Holds: orchestrator `AgentRuntime`, `InMemorySessionStore`, `System0ToolBroker`, `ThreadJournal`. Processes turns sequentially.

**System0ToolBroker** (implements `ToolBroker`) -- 4 built-in tools:
- `delegate_to_agent(agent_id, instruction)` -- creates/resumes a sub-agent thread, runs `AgentRuntime::execute`, journals all events
- `list_agents` -- returns configured agent IDs
- `read_config` -- returns config snapshot as JSON
- `modify_skill(skill_id, patch)` -- admin-only skill modification

**SkillToolBroker** (implements `ToolBroker`) -- Loads `SKILL.md` files via `SkillRegistry` (from `neuromancer-skills`), executes skill scripts with timeout, returns markdown/csv/script results.

**ThreadJournal** -- Append-only JSONL event log at `~/.local/neuromancer/threads/`. Redacts secret values on write. Directory layout: `system0/system0.jsonl`, `subagents/<thread_id>.jsonl`, `index.jsonl`.

**LLM Clients** -- Three implementations of `LlmClient`:
- `RigLlmClient` -- production client via rig-core (Groq, OpenAI, Anthropic, etc.)
- `TwoStepMockLlmClient` -- deterministic two-step mock for testing (returns tool call then final response)
- `EchoLlmClient` -- simple echo fallback

## Crate Relationships

```
neuromancer-core          Pure trait contracts + domain types (no rig-core dep)
    |                     Defines: ToolBroker, MemoryStore, SecretsBroker, PolicyEngine
    |
neuromancer-agent         Execution engine (depends on rig-core)
    |                     Implements: AgentRuntime (Think->Act loop),
    |                     ConversationContext, LlmClient trait, InMemorySessionStore
    |
neuromancerd              Integration layer (this crate)
                          Instantiates AgentRuntime per agent + System0
                          Implements System0ToolBroker and SkillToolBroker
                          Manages turn queue, thread journals, admin API
```

Key insight: System0 is an `AgentRuntime` whose `ToolBroker` can delegate to other `AgentRuntime` instances -- it's agents all the way down.

## RPC Surface

Orchestrator methods:
- `orchestrator.turn` -- submit a user message, get orchestrator response
- `orchestrator.runs.list` -- list all delegated runs
- `orchestrator.runs.get` -- get a specific run by ID
- `orchestrator.runs.diagnose` -- diagnostics for a specific run
- `orchestrator.context.get` -- get current conversation context
- `orchestrator.threads.list` -- list all thread journals
- `orchestrator.threads.get` -- get events from a specific thread
- `orchestrator.threads.resurrect` -- resurrect a thread into active session
- `orchestrator.subagent.turn` -- send a message directly to a sub-agent thread
- `orchestrator.events.query` -- query thread events with filters
- `orchestrator.stats.get` -- runtime statistics

Admin methods:
- `admin.health` -- health check with version and uptime
- `admin.config.reload` -- trigger config hot-reload

Removed (return method-not-found): `task.submit`, `task.list`, `task.get`, `message.send`

## Persistence Hierarchy

| Layer | Storage | Survives Restart | Notes |
|-------|---------|------------------|-------|
| Sessions | In-memory (`InMemorySessionStore`) | No | `ConversationContext` per agent, lost on restart |
| Threads | Disk (JSONL) | Yes | `~/.local/neuromancer/threads/`, can be resurrected |
| Runs | In-memory + journaled | Partially | `DelegatedRun` records per turn, indexed by run_id |

## Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `TURN_TIMEOUT` | 180s | Max duration for a single orchestrator turn |
| `DEFAULT_SKILL_SCRIPT_TIMEOUT` | 5s | Max duration for skill script execution |
| `DEFAULT_THREAD_PAGE_LIMIT` | 100 | Default page size for thread event queries |
| `DEFAULT_EVENTS_QUERY_LIMIT` | 200 | Default limit for events query results |
| Turn channel buffer | 128 | `mpsc::channel` buffer for `TurnRequest` |
| Report channel buffer | 256 | `mpsc::channel` buffer for sub-agent reports |

## v0.1-alpha Stubs

These subsystems are referenced but stubbed out, pending sibling crate integration:
- Secrets broker (handle-based injection, zeroize after use)
- MCP client pool (tool caching, SSE + child process transports)
- Policy engine (pre/post gates on tool/LLM calls)
- Triggers (Discord gateway, cron scheduling)
- Container execution for sandboxed tool runs
- Crash recovery checkpoints
- Sub-agent report remediation

## SDS Guardrails

- Secrets never enter LLM context -- referenced by handle only, injected at tool execution time, zeroized after use
- System0 is the single ingress orchestrator -- no ambient message APIs
- Explicit, auditable, revocable capabilities per agent
- Least privilege by default -- sub-agents can only reduce from parent capabilities
