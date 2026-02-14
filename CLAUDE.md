# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Neuromancer v0.1-alpha runs a CLI-first System0 orchestrator runtime in `neuromancerd`. System0 is an LLM agent instance (built with `neuromancer-agent`) that mediates user/admin turns, config/policy context, and delegation to sub-agents.

**Key principle**: System0 is the single ingress orchestrator and conversation owner. Public ingress is turn-based (`orchestrator.turn`), while delegated runs are tracked internally.

## Build Commands

```bash
cargo build --workspace              # Build all crates
cargo test --all                     # Run all tests
cargo clippy --all                   # Lint
cargo fmt --check                    # Format check
cargo test -p neuromancer-core       # Test a single crate
cargo test -p neuromancer-agent -- test_name  # Run a specific test
```

The daemon binary is `neuromancerd`:
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
| `neuromancer-skills` | Skill loading from SKILL.md files with frontmatter metadata |
| `neuromancer-triggers` | Event sources: Discord gateway + cron scheduling |
| `neuromancer-memory-simple` | SQLite-backed `MemoryStore` implementation |
| `neuromancer-secrets` | AES-encrypted SQLite `SecretsBroker` with ACL enforcement |

## Architecture

### Control Flow
```
CLI (`neuroctl orchestrator turn`)
  → Admin JSON-RPC (`orchestrator.turn`)
    → System0 input-message queue (`neuromancerd::message_runtime`)
      → System0 `AgentRuntime::execute_turn(...)` (`neuromancer-agent`)
        → System0ToolBroker allowlisted tools (policy/trigger gated)
          → Delegation to sub-agent runtimes (internal task execution)
        ← Delegated run updates (`running` → `completed`/`failed`)
      ← `OrchestratorTurnResult { turn_id, response, delegated_runs }`
```

### Core Traits (in `neuromancer-core`)

These define the system's extension points — all in `neuromancer-core/src/`:

- **`ToolBroker`** (`tool.rs`) — Policy-enforced tool execution. Lists and calls tools scoped to an `AgentContext`.
- **`MemoryStore`** (`memory.rs`) — Partitioned persistent storage with TTL, tags, and pagination.
- **`SecretsBroker`** (`secrets.rs`) — ACL-gated secret resolution. Agents reference secrets by handle; plaintext is injected (EnvVar/Header/FileMount) and zeroized after use.
- **`PolicyEngine`** (`policy.rs`) — Stacked pre/post gates on tool calls and LLM calls.

### v0.1-alpha Runtime Decisions

- **No backward compatibility** for old message/task APIs and CLI command shapes.
- **Public ingress is turn-based only**: `orchestrator.turn`.
- **Public task APIs were removed**: `message.send`, `task.submit`, `task.list`, and `task.get`.
- **Single conversation history is owned by `neuromancer-agent`** via in-memory session store.
- **System0 uses allowlisted control-plane tools** and policy-gates actions by trigger type/capabilities.
- **Queue split**: one public input-message queue plus internal delegated-run tracking.

### RPC and CLI Surface

- JSON-RPC methods:
  - `orchestrator.turn`
  - `orchestrator.runs.list`
  - `orchestrator.runs.get`
  - `orchestrator.runs.diagnose`
  - `orchestrator.context.get`
  - `orchestrator.threads.list`
  - `orchestrator.threads.get`
  - `orchestrator.threads.resurrect`
  - `orchestrator.subagent.turn`
  - `orchestrator.events.query`
  - `orchestrator.stats.get`
  - `admin.health`
  - `admin.config.reload`
- CLI commands (`neuroctl`):
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

Removed methods should return JSON-RPC method-not-found.

### State Machines

**Delegated sub-agent task lifecycle** (`core/task.rs`): Queued → Dispatched → Running → Completed/Failed/Cancelled/Suspended

**Agent execution** (`core/agent.rs`): Initializing → Thinking → Acting → WaitingForInput → Evaluating → Completed/Failed/Suspended

> **Note**: The `AgentRuntime` loop in `neuromancer-agent/src/runtime.rs` currently exercises only Thinking → Acting → Completed. Other states (WaitingForInput, Evaluating, Suspended) are defined in the core enum for future use.

### Error Hierarchy (`core/error.rs`)

`NeuromancerError` wraps domain-specific variants: `AgentError`, `LlmError`, `ToolError`, `PolicyError`, `InfraError` — each with fine-grained sub-variants.

## Configuration

Single TOML file (`neuromancer.toml`) with sections: `[global]`, `[otel]`, `[secrets]`, `[memory]`, `[models.*]`, `[mcp_servers.*]`, `[a2a]`, `[orchestrator]`, `[agents.*]`, `[triggers]`, `[admin_api]`.

For System0, `[orchestrator]` config controls:
- executor model slot
- allowlisted capabilities
- `system_prompt_path` for the orchestrator SYSTEM.md
- max iterations

Canonical example: `defaults/bootstrap/neuromancer.toml`.
Install/bootstrap flow: `neuroctl install` (defaults to XDG config path), with optional override `--config <path>`. Use `--override-config` to replace the target config file from bootstrap defaults. Install also detects configured model providers and prompts for API keys (one per provider) when interactive.
XDG layout defaults resolve under `$XDG_CONFIG_HOME/neuromancer` (or `~/.config/neuromancer`) and `$XDG_DATA_HOME/neuromancer` (or `~/.local/neuromancer`). Provider API keys are currently stored under `$XDG_RUNTIME_HOME/neuromancer/provider_keys` (falling back to `$XDG_DATA_HOME/neuromancer/provider_keys` when `XDG_RUNTIME_HOME` is unset) as a temporary alpha behavior.
OS-specific keychain integration is a required follow-up and must replace plaintext runtime key files.
Install bootstraps by copying `defaults/bootstrap/` into the XDG config root (non-overwriting). Per-agent prompt files are then created at `agents/<agent_name>/SYSTEM.md` from `defaults/templates/agent/SYSTEM.md` when agents are configured.

## Key Dependencies

- **LLM**: `rig-core` 0.11 — agent framework with completion model abstraction
- **MCP**: `rmcp` 0.1 — Model Context Protocol client (SSE + child process transports)
- **Async**: `tokio` full runtime, `axum` 0.8 for HTTP
- **DB**: `sqlx` 0.8 with SQLite
- **Discord**: `twilight-*` 0.16 for gateway/HTTP/models
- **Observability**: `tracing` + `opentelemetry` + `opentelemetry-otlp`
- **Security**: `secrecy` 0.10, `zeroize` 1.x for secret handling

## Reference

The system design specification is at `docs/initial_sds.md` (~71 KB) — the authoritative reference for architecture, protocols, and requirements.
