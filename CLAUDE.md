# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Neuromancer v0.1-alpha runs a CLI-first System0 orchestrator runtime in `neuromancerd`. System0 is an LLM agent instance (built with `neuromancer-agent`) that mediates user/admin turns, config/policy context, and delegation to sub-agents.

Key principle: System0 is the single ingress orchestrator and conversation owner. Public ingress is turn-based (`orchestrator.turn`), while delegated runs are tracked internally.

## Build Commands

```bash
cargo build --workspace
cargo test --all
cargo clippy --all
cargo fmt --check
cargo test -p neuromancer-core
cargo test -p neuromancer-agent -- test_name
```

**Requires Rust 1.85+** (edition 2024).

Daemon binary:

```bash
cargo run -p neuromancerd -- -c neuromancer.toml
```

## Workspace Structure (10 crates)

| Crate | Role |
|-------|------|
| `neuromancer-core` | Shared traits and types only (no implementations) |
| `neuromancer-agent` | Rig-based agent runtime with state machine execution |
| `neuromancer-cli` | `neuroctl` admin/ops CLI over JSON-RPC |
| `neuromancerd` | Daemon binary: config, admin API, System0 runtime, telemetry, shutdown |
| `neuromancer-a2a` | Agent-to-Agent HTTP protocol (Agent Card, tasks, SSE streaming) |
| `neuromancer-mcp` | MCP client pool — manages MCP server lifecycle and tool caching |
| `neuromancer-skills` | Skill loading from `SKILL.md` files with frontmatter metadata |
| `neuromancer-triggers` | Event sources: Discord gateway + cron scheduling |
| `neuromancer-memory-simple` | SQLite-backed `MemoryStore` implementation |
| `neuromancer-secrets` | AES-encrypted SQLite `SecretsBroker` with ACL enforcement |

## Runtime Architecture

### Control Flow

```text
CLI (`neuroctl orchestrator turn`)
  -> Admin JSON-RPC (`orchestrator.turn`)
    -> `neuromancerd::orchestrator::System0Runtime::turn`
      -> turn queue (`mpsc<TurnRequest>`)
        -> `System0TurnWorker::process_turn`
          -> System0 `AgentRuntime::execute_turn_with_thread_store(...)`
            -> `System0ToolBroker` -> `actions::dispatch::dispatch_tool`
              -> runtime/adaptive/authenticated_adaptive action handlers
          -> thread/event journaling + delegated run updates
      <- `OrchestratorTurnResult { turn_id, response, delegated_tasks, tool_invocations }`
```

### Orchestrator Internals

See `neuromancerd/CLAUDE.md` for the full orchestrator module map. Key entry points:

- `neuromancerd/src/orchestrator/runtime/mod.rs`: `System0Runtime` public API, `ToolBroker` impl for `System0ToolBroker`
- `neuromancerd/src/orchestrator/runtime/builder.rs`: sub-agent construction, `SqliteThreadStore` + System0 `AgentThread` creation, worker spawning
- `neuromancerd/src/orchestrator/actions/dispatch.rs`: tool-class dispatch (Runtime / Adaptive / AuthenticatedAdaptive)
- `neuromancerd/src/orchestrator/state/mod.rs`: `System0ToolBroker` state, tool spec registry, `Arc<dyn ThreadStore>`

### Self-Improvement Security Model

- No transport auth mechanism is implemented yet; privilege boundary is `TriggerType`.
- In current CLI/chat ingress, `orchestrator.turn` is processed as `TriggerType::Admin`.
- Admin enforcement for mutating lifecycle steps is centralized in `security/trigger_gate.rs`.
- `authenticated_adaptive` actions hard-fail for non-admin trigger when required.
- `modify_skill` is a compatibility alias that routes through proposal verification/audit/apply flow; it does not bypass lifecycle checks.
- `ExecutionGuard` hooks (`pre_verify_proposal`, `pre_apply_proposal`, `pre_skill_script_execution`) fail closed with `blocked_missing_sandbox` for unsupported safeguards.

### Proposal Lifecycle

`ProposalState` progression:

`proposal_created -> verification_passed|verification_failed -> audit_passed|audit_blocked -> awaiting_admin_message -> authorized -> applied_canary -> promoted|rolled_back`

High/critical risk defaults to blocked for authorization unless explicitly forced in an admin turn.

### Tool Classes

- Runtime tools: `delegate_to_agent`, `list_agents`, `read_config`, `queue_status`
- Adaptive tools: `list_proposals`, `get_proposal`, `propose_config_change`, `propose_skill_add`, `propose_skill_update`, `propose_agent_add`, `propose_agent_update`, `analyze_failures`, `score_skills`, `adapt_routing`, `record_lesson`, `run_redteam_eval`, `list_audit_records`
- Authenticated adaptive tools: `authorize_proposal`, `apply_authorized_proposal`, `modify_skill`

## Core Traits and Types (in `neuromancer-core`)

- `ToolBroker` (`tool.rs`) — policy-enforced tool execution scoped to `AgentContext`.
- `ThreadStore` (`thread.rs`) — async trait for persistent thread/message storage (SQLite-backed via `SqliteThreadStore` in neuromancerd).
- `MemoryStore` (`memory.rs`) — partitioned persistent storage with TTL, tags, pagination.
- `SecretsBroker` (`secrets.rs`) — ACL-gated secret resolution with handle-based use.
- `PolicyEngine` (`policy.rs`) — stacked pre/post gates on tool calls and LLM calls.

### Thread Infrastructure (`thread.rs`)

`AgentThread` is the unified conversation record for all agent interactions. Key types:

- **`AgentThread`** — persistent thread record: `id`, `agent_id`, `scope`, `compaction_policy`, `context_window_budget`, `status`, timestamps
- **`ThreadScope`** — `System0`, `Task { task_id }`, `UserConversation { conversation_id }`, `Collaboration { parent_thread_id, root_scope }`
- **`ThreadStatus`** — `Active`, `Completed`, `Failed`, `Suspended`
- **`CompactionPolicy`** — `SummarizeToMemory { target_partition, summarizer_model, threshold_pct }`, `InPlace { strategy }`, `None`
- **`UserConversation`** — persistent user-agent chat session record: `conversation_id`, `agent_id`, `thread_id`, `status`, timestamps
- **`ThreadStore`** trait — CRUD for threads + `append_messages` / `load_messages` for conversation persistence, `find_collaboration_thread`, `mark_compacted`, `find_user_conversation` / `save_user_conversation` / `list_user_conversations`
- **`ChatMessage`** — message types (`MessageRole`, `MessageContent`, `MessageMetadata`) used by both agent runtime and thread store

Message types (`ChatMessage`, `MessageRole`, `MessageContent`, `TruncationStrategy`, etc.) are defined in `neuromancer-core/src/thread.rs` and re-exported by `neuromancer-agent/src/conversation.rs`.

## RPC and CLI Surface

JSON-RPC methods:

- `orchestrator.turn`
- `orchestrator.runs.list`
- `orchestrator.runs.get`
- `orchestrator.runs.diagnose`
- `orchestrator.outputs.pull`
- `orchestrator.context.get`
- `orchestrator.threads.list`
- `orchestrator.threads.get`
- `orchestrator.threads.resurrect`
- `orchestrator.subagent.turn`
- `orchestrator.chat.turn`
- `orchestrator.chat.list`
- `orchestrator.events.query`
- `orchestrator.stats.get`
- `admin.health`
- `admin.config.reload`

CLI commands (`neuroctl`):

- `install [--config <path>] [--override-config]`
- `daemon {start,restart,stop,status}`
- `health`
- `config reload`
- `rpc call --method <method> [--params <json>]`
- `e2e smoke`
- `orchestrator turn "<message>"`
- `orchestrator chat`
- `orchestrator runs {list, get <run_id>, diagnose <run_id>}`
- `orchestrator events query [--thread-id ...] [--run-id ...] [--agent-id ...] [--tool-id ...] [--event-type ...] [--error-contains ...]`
- `orchestrator stats get`
- `orchestrator threads {list, get <thread_id>, resurrect <thread_id>}`
- `orchestrator subagent turn --thread-id <id> "<message>"`
- `chat <agent_id> -m "<message>"` (UserConversation: direct agent chat, bypasses System0)
- `chat --list` (list active UserConversation sessions)

Removed methods should return JSON-RPC method-not-found (`message.send`, `task.submit`, `task.list`, `task.get`).

## Configuration

Single TOML file (`neuromancer.toml`) with sections:
`[global]`, `[otel]`, `[secrets]`, `[memory]`, `[models.*]`, `[mcp_servers.*]`, `[a2a]`, `[orchestrator]`, `[agents.*]`, `[triggers]`, `[admin_api]`.

### Model Slots (`[models.*]`)

Each model slot specifies a `provider`, `model`, and optional `base_url`. All non-mock providers use rig's OpenAI-compatible client.

Known providers with built-in defaults:

| Provider | Default `base_url` | API key env var |
|----------|-------------------|-----------------|
| `openai` | *(rig default)* | `OPENAI_API_KEY` |
| `groq` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `fireworks` | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` |
| `xai` | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `mistral` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` |

The `base_url` field overrides the provider default, enabling custom/proxy endpoints. Unknown providers require an explicit `base_url`.

```toml
[models.executor]
provider = "fireworks"
model = "accounts/fireworks/models/llama-v3p3-70b-instruct"

[models.custom]
provider = "openai"
base_url = "https://my-proxy.example.com/v1"
model = "my-model"
```

For System0, `[orchestrator]` controls:

- model slot
- capabilities allowlist (System0 tools)
- `system_prompt_path`
- max iterations
- `self_improvement` (`enabled`, `audit_agent_id`, admin-gating toggle, verify/canary toggles, thresholds)

When self-improvement is enabled, config validation requires the configured audit agent to exist in `[agents]`.

Canonical example: `defaults/bootstrap/neuromancer.toml`.

## Key Dependencies

- LLM: `rig-core` 0.11
- MCP: `rmcp` 0.1
- Async/HTTP: `tokio`, `axum` 0.8
- DB: `sqlx` 0.8 (SQLite)
- Discord: `twilight-*` 0.16
- Observability: `tracing`, `opentelemetry`, `opentelemetry-otlp`
- Security primitives: `secrecy`, `zeroize`

## Reference

System design specification: `docs/initial_sds.md`.

## Referencing Crates

Whenever you are looking for how crates are implemented, ALWAYS go directly to crates.io
