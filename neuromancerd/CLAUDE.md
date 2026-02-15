# neuromancerd

Daemon binary hosting the System0 orchestrator runtime. It loads config, starts the admin API (`axum`), runs orchestrator turns through a worker queue, delegates sub-agent turns, and journals thread events.

System0 is itself an `AgentRuntime` (`neuromancer-agent`) whose `ToolBroker` can delegate to other `AgentRuntime` instances.

## Build and Test

```bash
cargo build -p neuromancerd
cargo test -p neuromancerd
cargo run -p neuromancerd -- -c neuromancer.toml
cargo run -p neuromancerd -- --validate -c neuromancer.toml
```

## Top-Level Module Map

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI args, boot sequence, config watcher, admin server spawn, shutdown loop |
| `src/config.rs` | TOML load/validate + hot-reload watcher (`notify`) |
| `src/admin.rs` | JSON-RPC dispatch and admin HTTP endpoints |
| `src/shutdown.rs` | signal handling (shutdown/reload) |
| `src/telemetry.rs` | tracing/OTEL setup |
| `src/orchestrator/` | orchestrator runtime and all runtime internals |

## Orchestrator Module Structure

`src/orchestrator/mod.rs` exports `OrchestratorRuntime` and `OrchestratorRuntimeError` and includes:

### Runtime Core

- `runtime.rs`: `OrchestratorRuntime`, turn queue worker, orchestrator RPC helpers, thread/runs/context APIs.
- `state.rs`: `System0ToolBroker` state, tool spec registry, invocation recording, proposal creation helpers.
- `bootstrap.rs`: build orchestrator agent config.
- `prompt.rs`: prompt loading/rendering.
- `llm_clients.rs`: LLM client builders and retry-limit resolution.
- `tools.rs`: default/effective System0 tool allowlist.
- `error.rs`: orchestrator runtime error variants.

### Actions

- `actions/dispatch.rs`: classifies tool calls into `Runtime`, `Adaptive`, `AuthenticatedAdaptive`.
- `actions/runtime_actions.rs`: runtime operations (`delegate_to_agent`, `list_agents`, `read_config`).
- `actions/adaptive_actions.rs`: non-admin adaptive operations (proposal creation/list/read, analytics, lessons, red-team, audit log reads).
- `actions/authenticated_adaptive_actions.rs`: admin-authorized lifecycle mutations (`authorize_proposal`, `apply_authorized_proposal`) and `modify_skill` compatibility alias.

### Security

- `security/trigger_gate.rs`: central `TriggerType::Admin` enforcement helper.
- `security/audit.rs`: risk scoring, safeguards, allow/block verdicts, mutation audit record model.
- `security/execution_guard.rs`: `ExecutionGuard` hook abstraction; fail-closed `blocked_missing_sandbox`.
- `security/redaction.rs`: secret masking + payload shape restrictions for tool events.

### Proposals

- `proposals/model.rs`: proposal kind/state/report/authorization/apply models.
- `proposals/verification.rs`: verification checks (including skill lint and unknown-skill checks on agent patches).
- `proposals/lifecycle.rs`: hash generation + state transitions.
- `proposals/apply.rs`: mutation application to managed config/skills/agents.

### Adaptation

- `adaptation/analytics.rs`: failure clustering, skill quality scoring, routing recommendations.
- `adaptation/canary.rs`: canary metrics and regression rollback decision.
- `adaptation/lessons.rs`: lessons partition constant.
- `adaptation/redteam.rs`: lightweight continuous red-team report.

### Tracing

- `tracing/thread_journal.rs`: append/read JSONL thread events + index snapshots.
- `tracing/event_query.rs`: query filtering helpers.
- `tracing/conversation_projection.rs`: conversation reconstruction and timeline conversion.
- `tracing/jsonl_io.rs`: low-level JSONL read/write.

### Skills

- `skills/broker.rs`: skill tool broker implementation.
- `skills/script_runner.rs`: skill script execution.
- `skills/path_policy.rs`: filesystem path policy checks.
- `skills/csv.rs`, `skills/aliases.rs`: parsing/alias helpers.

## Turn Processing Flow

```text
neuroctl orchestrator turn "message"
  -> POST /rpc (`admin.rs`) -> `orchestrator.turn`
    -> `OrchestratorRuntime::orchestrator_turn`
      -> enqueue `TurnRequest { message, trigger_type }` on mpsc
        -> worker: `RuntimeCore::process_turn`
          -> journal `message_user`
          -> System0 `AgentRuntime::execute_turn(..., TriggerSource::Cli, ...)`
            -> `System0ToolBroker.call_tool` -> dispatch -> action handler
          -> journal tool invocations + `message_assistant`
      <- `OrchestratorTurnResult { turn_id, response, delegated_runs, tool_invocations }`
```

Current ingress behavior: orchestrator turns are enqueued with `TriggerType::Admin` (CLI/chat boundary).

## Self-Improvement Lifecycle and Gating

Self-improvement tools are enabled by `orchestrator.self_improvement.enabled`.

Lifecycle states:

`proposal_created -> verification_passed|verification_failed -> audit_passed|audit_blocked -> awaiting_admin_message -> authorized -> applied_canary -> promoted|rolled_back`

Rules implemented in runtime:

- `authorize_proposal` and `apply_authorized_proposal` use `trigger_gate::ensure_admin_trigger`.
- Verification must pass before authorization when `verify_before_authorize = true`.
- High/critical-risk proposals require explicit `force=true` in admin turns.
- `ExecutionGuard::pre_apply_proposal` can block and force rollback.
- Canary regression detection can rollback before promotion (`canary_before_promote` thresholds).
- Every mutation path records `MutationAuditRecord` with trigger type, proposal id/hash, outcome, and details.
- `modify_skill` is compatibility only and still flows through proposal verify/audit/guard/apply logic.

No transport authentication mechanism is currently added; `TriggerType` is the effective privilege boundary.

## Config Validation Hooks

`src/config.rs` enforces:

- referenced model slots exist
- agent references are valid (`mcp_servers`, `a2a_peers`)
- cron template agent references are valid
- prompt files exist and are valid markdown
- if `orchestrator.self_improvement.enabled = true`, the configured `audit_agent_id` must exist in `[agents]`

## RPC Surface

See root `CLAUDE.md` for the full RPC method list. Dispatch is in `src/admin.rs`.

## Tests to Keep in Mind

`src/orchestrator/runtime_tests.rs` covers core self-improvement security and lifecycle behavior, including:

- non-admin denial for `authorize_proposal` and `apply_authorized_proposal`
- admin authorize/apply happy path
- dangerous proposal blocking
- unknown-skill rejection in agent updates
- canary rollback behavior
- mutation audit logging expectations
- `modify_skill` compatibility path behavior
