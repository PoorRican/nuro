# Initial SDS Discrepancy Report (Implemented Behavior Only)

## Scope

This report compares `docs/initial_sds.md` to the current codebase and includes only discrepancies in behavior that is actually implemented. Obvious stubs/TODO placeholders were excluded.

## Verdict

`docs/initial_sds.md` is **not fully up to date** with implemented behavior.

## Discrepancies

| ID | Area | SDS expectation | Implemented behavior | Evidence |
|---|---|---|---|---|
| D-001 | LLM caller boundary | Sub-agents are described as the only components that call LLMs. | System0/orchestrator is itself instantiated as an `AgentRuntime` with an LLM client and executes turns through `execute_turn`. | SDS: `docs/initial_sds.md:128`, `docs/initial_sds.md:140`.<br>Code: `neuromancerd/src/orchestrator/runtime.rs:403`, `neuromancerd/src/orchestrator/runtime.rs:111`. |
| D-002 | Capability enforcement surface | Sub-agent capability set includes filesystem roots and outbound network policy. | Those fields exist in `AgentCapabilities`, but `AgentContext` (what tools actually receive/enforce) does not carry them. Skill file access is rooted in a global runtime root, not per-agent filesystem roots. | SDS: `docs/initial_sds.md:130`, `docs/initial_sds.md:137`, `docs/initial_sds.md:138`.<br>Code: `neuromancer-core/src/agent.rs:21`, `neuromancer-core/src/agent.rs:22`, `neuromancer-core/src/tool.rs:49`, `neuromancer-agent/src/runtime.rs:67`, `neuromancerd/src/orchestrator/runtime.rs:269`, `neuromancerd/src/orchestrator/skills/broker.rs:170`. |
| D-003 | Delegation context shape | Sub-agent dispatch should inject memory items and a parent-context summary slice into a fresh sub-agent conversation. | Delegation executes with the instruction only; the turn conversation is initialized as system prompt + user message, with no memory/context-slice injection path in this flow. | SDS: `docs/initial_sds.md:494`, `docs/initial_sds.md:498`, `docs/initial_sds.md:499`.<br>Code: `neuromancerd/src/orchestrator/actions/runtime_actions.rs:419`, `neuromancerd/src/orchestrator/actions/runtime_actions.rs:423`, `neuromancer-agent/src/runtime.rs:289`, `neuromancer-agent/src/runtime.rs:302`. |
| D-004 | Runtime tool list | Runtime tool class is documented as `delegate_to_agent`, `list_agents`, `read_config`. | Runtime tools additionally include `queue_status`, and it is exposed as a built-in tool spec. | SDS: `docs/initial_sds.md:180`, `docs/initial_sds.md:757`.<br>Code: `neuromancerd/src/orchestrator/actions/runtime_actions.rs:19`, `neuromancerd/src/orchestrator/actions/runtime_actions.rs:23`, `neuromancerd/src/orchestrator/state.rs:290`. |
| D-005 | Admin JSON-RPC method catalog | Method table does not include an outputs-poll endpoint. | `orchestrator.outputs.pull` is implemented in daemon dispatch and exposed in CLI command surface. | SDS: `docs/initial_sds.md:1814`, `docs/initial_sds.md:1830`.<br>Code: `neuromancerd/src/admin.rs:402`, `neuromancer-cli/src/cli.rs:181`, `neuromancer-cli/src/cli.rs:221`. |
| D-006 | Key handling directive | v0.1 directive states keychain integration must replace temporary plaintext runtime key files. | Install flow still writes provider keys as plaintext runtime files and explicitly warns keychain integration is a follow-up. | SDS: `docs/initial_sds.md:26`, `docs/initial_sds.md:27`.<br>Code: `neuromancer-cli/src/install.rs:151`, `neuromancer-cli/src/install.rs:165`, `neuromancer-cli/src/install.rs:172`, `neuromancer-cli/src/provider_keys.rs:89`. |

