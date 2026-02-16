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
    -> `neuromancerd::orchestrator::OrchestratorRuntime::orchestrator_turn`
      -> turn queue (`mpsc<TurnRequest>`)
        -> `RuntimeCore::process_turn`
          -> System0 `AgentRuntime::execute_turn(...)`
            -> `System0ToolBroker` -> `actions::dispatch::dispatch_tool`
              -> runtime/adaptive/authenticated_adaptive action handlers
          -> thread/event journaling + delegated run updates
      <- `OrchestratorTurnResult { turn_id, response, delegated_runs, tool_invocations }`
```

### Orchestrator Internals

See `neuromancerd/CLAUDE.md` for the full orchestrator module map. Key entry points:

- `neuromancerd/src/orchestrator/runtime.rs`: `OrchestratorRuntime` public API and turn worker
- `neuromancerd/src/orchestrator/actions/dispatch.rs`: tool-class dispatch (Runtime / Adaptive / AuthenticatedAdaptive)
- `neuromancerd/src/orchestrator/state.rs`: `System0ToolBroker` state and tool spec registry

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

- Runtime tools: `delegate_to_agent`, `list_agents`, `read_config`
- Adaptive tools: `list_proposals`, `get_proposal`, `propose_config_change`, `propose_skill_add`, `propose_skill_update`, `propose_agent_add`, `propose_agent_update`, `analyze_failures`, `score_skills`, `adapt_routing`, `record_lesson`, `run_redteam_eval`, `list_audit_records`
- Authenticated adaptive tools: `authorize_proposal`, `apply_authorized_proposal`, `modify_skill`

## Core Traits (in `neuromancer-core`)

- `ToolBroker` (`tool.rs`) — policy-enforced tool execution scoped to `AgentContext`.
- `MemoryStore` (`memory.rs`) — partitioned persistent storage with TTL, tags, pagination.
- `SecretsBroker` (`secrets.rs`) — ACL-gated secret resolution with handle-based use.
- `PolicyEngine` (`policy.rs`) — stacked pre/post gates on tool calls and LLM calls.

## RPC and CLI Surface

JSON-RPC methods:

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

Removed methods should return JSON-RPC method-not-found (`message.send`, `task.submit`, `task.list`, `task.get`).

## Configuration

Single TOML file (`neuromancer.toml`) with sections:
`[global]`, `[otel]`, `[secrets]`, `[memory]`, `[models.*]`, `[mcp_servers.*]`, `[a2a]`, `[orchestrator]`, `[agents.*]`, `[triggers]`, `[admin_api]`.

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
