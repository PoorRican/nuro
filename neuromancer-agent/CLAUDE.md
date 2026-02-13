# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo test -p neuromancer-agent            # Run all tests in this crate
cargo test -p neuromancer-agent -- test_name  # Run a specific test
cargo clippy -p neuromancer-agent          # Lint this crate
cargo build -p neuromancer-agent           # Build this crate only
cargo build --workspace                    # Build all crates
cargo test --all                           # Run all workspace tests
```

## What This Crate Does

`neuromancer-agent` is the agent execution runtime — it takes a `Task` and runs it through a deterministic Thinking→Acting loop until the LLM produces a final text response (no more tool calls). It is **not** a planner; it is a supervised executor.

Entry point: `AgentRuntime::execute(&self, task: &mut Task) -> Result<TaskOutput, NeuromancerError>`

## Module Map

| File | Purpose |
|------|---------|
| `runtime.rs` | `AgentRuntime` — the state machine loop (Initializing→Thinking→Acting→Completed) |
| `conversation.rs` | `ConversationContext` + `ChatMessage` — token-budgeted message buffer with truncation |
| `llm.rs` | `LlmClient` trait + `RigLlmClient<M>` adapter + `MockLlmClient` for tests |
| `model_router.rs` | `ModelRouter` — resolves agent role slots to `ModelSlotConfig` |

## Architecture

### Execution Loop (`runtime.rs`)

```
AgentRuntime::execute(task)
  1. Build AgentContext (security scope: allowed tools, secrets, memory partitions)
  2. Create ConversationContext (128K token budget, SlidingWindow{keep_last: 50})
  3. Add system prompt + task instruction
  4. Loop (max_iterations guard):
     a. Thinking: llm_client.complete(prompt, history, tool_defs) → LlmResponse
     b. If tool_calls → Acting: tool_broker.call_tool() for each, add results to conversation
     c. If text only → break, return TaskOutput
     d. Every 5 iterations: send SubAgentReport::Progress to orchestrator
  5. On completion: create Checkpoint, send SubAgentReport::Completed
```

Tool failures are non-fatal — the error is added to conversation context so the LLM can self-correct.

### rig-core Integration (`llm.rs`)

The crate does **not** use rig's `Agent` type. It wraps rig's `CompletionModel` trait via `RigLlmClient<M>`, keeping the custom state machine independent of rig's agent lifecycle. Token counts from rig are currently placeholder zeros.

### Conversation Management (`conversation.rs`)

- Token estimation: `text.len() / 4` (chars-per-token heuristic), 50 tokens per tool call
- Truncation triggers when `token_used > token_budget`
- Strategies: `SlidingWindow` (default, keeps last N non-system messages), `Strict` (drops oldest), `Summarize` (stub, falls back to Strict)
- System messages are always preserved during truncation
- `to_rig_messages()` converts to rig's `Message` format (skips system messages, which are provided via `system_prompt`)

### Model Resolution (`model_router.rs`)

Resolution order for `resolve_for_agent(role, agent_models)`:
1. Check agent-level override (`agent_models.{role}` → slot name)
2. Fall back to role name as slot name
3. Look up slot in global `[models.*]` config
4. Error if not found (no silent defaults)

## Key Types from `neuromancer-core`

These are the main types this crate consumes — defined in the workspace root's `neuromancer-core`:

- **`AgentConfig`** — identity, mode, capabilities, limits, model slots, system_prompt, max_iterations
- **`AgentContext`** — request-scoped security context (agent_id, task_id, allowed_tools, allowed_secrets, etc.)
- **`Task`** / **`TaskState`** / **`TaskOutput`** — work unit lifecycle and results
- **`ToolBroker`** trait — `list_tools(ctx)` and `call_tool(ctx, call)` (policy-gated)
- **`ToolSpec`** / **`ToolCall`** / **`ToolResult`** — tool metadata, invocations, and results
- **`SubAgentReport`** — enum sent to orchestrator: Progress, Stuck, ToolFailure, Completed, Failed
- **`NeuromancerError`** — domain error hierarchy (AgentError, LlmError, ToolError, PolicyError, InfraError)

## Testing Conventions

Tests use `MockLlmClient` (pre-loaded response sequence) and `MockToolBroker` (always succeeds) with `tokio::sync::mpsc::channel` to capture `SubAgentReport` messages:

```rust
#[tokio::test]
async fn test_something() {
    let mock_llm = Arc::new(MockLlmClient::new(vec![/* LlmResponse sequence */]));
    let mock_broker = Arc::new(MockToolBroker);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    let runtime = AgentRuntime::new(test_config(), mock_llm, mock_broker, tx);
    let mut task = Task::new("task-1".into(), "do something".into());
    let output = runtime.execute(&mut task).await.unwrap();
    // Assert on output, task.state, rx.try_recv() for reports
}
```

`test_config()` helper returns an `AgentConfig` with sensible defaults (max_iterations: 5, mode: Inproc).
