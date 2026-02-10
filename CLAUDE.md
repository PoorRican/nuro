# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Neuromancer is a deterministic orchestrator daemon that supervises isolated rig-powered sub-agents. It uses a hybrid control plane (deterministic routing + LLM-assisted fallback) with explicit security boundaries, encrypted secrets, partitioned memory, and OpenTelemetry observability.

**Key principle**: The orchestrator is a supervisor, not a planner. Sub-agents (via `rig`) perform all reasoning. The orchestrator routes, supervises, and remediates.

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

## Workspace Structure (11 crates)

| Crate | Role |
|-------|------|
| `neuromancer-core` | Shared traits and types only (no implementations) |
| `neuromancer-orchestrator` | Supervisor: routing, task queue, agent registry, remediation |
| `neuromancer-agent` | Rig-based agent runtime with state machine execution |
| `neuromancer-a2a` | Agent-to-Agent HTTP protocol (Agent Card, tasks, SSE streaming) |
| `neuromancer-mcp` | MCP client pool — manages MCP server lifecycle and tool caching |
| `neuromancer-skills` | Skill loading from SKILL.md files with frontmatter metadata |
| `neuromancer-triggers` | Event sources: Discord gateway + cron scheduling |
| `neuromancer-memory-simple` | SQLite-backed `MemoryStore` implementation |
| `neuromancer-secrets` | AES-encrypted SQLite `SecretsBroker` with ACL enforcement |
| `neuromancerd` | Daemon binary: config, admin API, telemetry, shutdown |

## Architecture

### Control Flow
```
Triggers (Discord/Cron/A2A/Admin)
  → Orchestrator (TaskQueue → Router → AgentRegistry)
    → AgentRuntime (state machine: Initializing→Thinking→Acting→Evaluating)
      → ToolBroker (Skills/MCP/A2A tools, policy-gated)
      → SecretsBroker (handle-based, never in LLM context)
      → MemoryStore (partition-scoped)
    ← SubAgentReport (back to orchestrator for remediation)
```

### Core Traits (in `neuromancer-core`)

These define the system's extension points — all in `neuromancer-core/src/`:

- **`ToolBroker`** (`tool.rs`) — Policy-enforced tool execution. Lists and calls tools scoped to an `AgentContext`.
- **`MemoryStore`** (`memory.rs`) — Partitioned persistent storage with TTL, tags, and pagination.
- **`SecretsBroker`** (`secrets.rs`) — ACL-gated secret resolution. Agents reference secrets by handle; plaintext is injected (EnvVar/Header/FileMount) and zeroized after use.
- **`PolicyEngine`** (`policy.rs`) — Stacked pre/post gates on tool calls and LLM calls.

### Key Design Decisions

- **Three protocol standards, no custom ones**: MCP (tools), A2A (agent delegation), Skills (instructions as SKILL.md).
- **Secrets never enter LLM context** — handle-based injection only, zeroized in memory via `secrecy`/`zeroize`.
- **Deterministic routing first** — LLM classifier is fallback only, for ambiguous inputs.
- **Circuit breaker pattern** in `AgentRegistry` prevents cascading agent failures.
- **Partitioned memory** — agents only access explicitly granted partitions.
- **Remediation policies** — orchestrator handles agent failures (retry, reassign, escalate, abort).

### State Machines

**Task lifecycle** (`core/task.rs`): Queued → Dispatched → Running → Completed/Failed/Cancelled/Suspended

**Agent execution** (`core/agent.rs`): Initializing → Thinking → Acting → WaitingForInput → Evaluating → Completed/Failed/Suspended

### Error Hierarchy (`core/error.rs`)

`NeuromancerError` wraps domain-specific variants: `AgentError`, `LlmError`, `ToolError`, `PolicyError`, `InfraError` — each with fine-grained sub-variants.

## Configuration

Single TOML file (`neuromancer.toml`) with sections: `[global]`, `[otel]`, `[secrets]`, `[memory]`, `[models.*]`, `[mcp_servers.*]`, `[a2a]`, `[routing]`, `[agents.*]`, `[triggers]`, `[admin_api]`. Config structs are in `neuromancer-core/src/config.rs` and `neuromancerd/src/config.rs`.

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
