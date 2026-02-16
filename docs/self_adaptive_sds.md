# System Design Specification: Self-Adaptive Runtime

**Neuromancer v0.1-alpha | neuromancerd**

**Status:** Implemented (v0.0.2-alpha)
**Last Updated:** 2026-02-14

---

## 1. Overview

The self-adaptive runtime enables System0 (the orchestrator agent) to introspect its own operational state, propose mutations to its configuration, skills, and agent topology, and apply those mutations through a multi-stage, admin-gated lifecycle. The system is designed around a single invariant: **no mutation is ever automatic**. Every change flows through a proposal pipeline with verification, audit, authorization, canary testing, and promotion stages, each with independent failure modes and rollback paths.

### 1.1 Design Goals

1. **Controlled evolution** -- System0 can observe failure patterns, score skill quality, and propose improvements, but cannot unilaterally enact them.
2. **Admin-gated mutations** -- All state-mutating operations require `TriggerType::Admin` privilege, enforced at a single chokepoint.
3. **Fail-closed safety** -- Execution guards block operations when required safeguards (e.g., sandboxing) are not implemented, rather than degrading silently.
4. **Full audit trail** -- Every mutation attempt (successful or not) produces a `MutationAuditRecord` with trigger type, proposal identity, outcome, and details.
5. **Automatic regression detection** -- Canary metrics are compared against configurable thresholds before promotion; regressions trigger automatic rollback.

### 1.2 Scope

This specification covers:

- The proposal lifecycle state machine
- Tool class hierarchy and dispatch
- Security model (trigger gating, audit, execution guards, redaction)
- Adaptation analytics (failure clustering, skill scoring, routing)
- Canary testing and rollback mechanics
- Skill management (linting, registry, script execution, path policy)
- Configuration surface and validation
- Managed runtime state

It does **not** cover the broader orchestrator runtime (turn processing, thread journaling, agent delegation), which are documented in the initial SDS.

---

## 2. Architecture

### 2.1 Component Map

```
neuromancerd/src/orchestrator/
  actions/
    dispatch.rs          -- Tool class classification and top-level dispatch
    adaptive_actions.rs  -- Non-mutating self-improvement tool handlers
    authenticated_adaptive_actions.rs  -- Admin-gated mutation handlers
    runtime_actions.rs   -- Core orchestrator tools (delegate, list, config)
  proposals/
    model.rs             -- Proposal data model and lifecycle states
    verification.rs      -- Pre-authorization verification checks
    lifecycle.rs         -- State transitions and deterministic hashing
    apply.rs             -- Mutation application to managed state
  security/
    trigger_gate.rs      -- Central TriggerType::Admin enforcement
    audit.rs             -- Risk scoring, exploit detection, audit records
    execution_guard.rs   -- Fail-closed safeguard hooks
    redaction.rs         -- Secret masking for event payloads
  adaptation/
    analytics.rs         -- Failure clustering, skill scoring, routing adaptation
    canary.rs            -- Canary metrics collection and rollback decisions
    lessons.rs           -- Lessons-learned memory partition
    redteam.rs           -- Lightweight red-team evaluation
  skills/
    broker.rs            -- SkillToolBroker (ToolBroker impl for skills)
    script_runner.rs     -- Sandboxed Python script execution
    path_policy.rs       -- Filesystem path traversal prevention
    aliases.rs           -- Skill tool alias resolution
    csv.rs               -- CSV data source parsing
  state.rs               -- System0BrokerInner (all managed mutable state)
  tools.rs               -- Default tool allowlist

neuromancer-skills/src/
  lib.rs                 -- Skill struct, SkillError
  metadata.rs            -- SKILL.md frontmatter parser
  linter.rs              -- Dangerous pattern detection
  registry.rs            -- Skill directory scanner and progressive loader

neuromancer-core/src/
  config.rs              -- SelfImprovementConfig, SelfImprovementThresholds
  trigger.rs             -- TriggerType, TriggerSource, Actor
  tool.rs                -- ToolBroker, ToolSpec, AgentContext
  policy.rs              -- PolicyEngine, PolicyDecision
```

### 2.2 Data Flow

```
User/Admin turn
  -> orchestrator.turn (TriggerType::Admin via CLI)
    -> System0 AgentRuntime executes LLM loop
      -> LLM selects tool call
        -> dispatch::dispatch_tool classifies tool
          -> ToolClass::Adaptive      => handle_adaptive_action
          -> ToolClass::AuthenticatedAdaptive => handle_authenticated_adaptive_action
          -> ToolClass::Runtime       => handle_runtime_action
          -> ToolClass::Unknown       => not_found_err

Proposal creation flow (Adaptive):
  propose_* -> create_proposal -> verify -> audit -> guard check -> state transitions
    -> stored in proposals_index, proposals_order

Mutation flow (AuthenticatedAdaptive):
  authorize_proposal -> ensure_admin_trigger -> verify check -> risk check -> authorize
  apply_authorized_proposal -> ensure_admin_trigger -> guard -> apply mutation
    -> canary metrics -> rollback_reason check -> promote or rollback
```

---

## 3. Proposal Lifecycle

### 3.1 State Machine

```
                    +--> VerificationFailed --+
                    |                         |
ProposalCreated ----+                         +--> AuditBlocked --> (terminal)
                    |                         |
                    +--> VerificationPassed ---+
                                              |
                                              +--> AuditPassed
                                                     |
                                                     v
                                              AwaitingAdminMessage
                                                     |
                                              (admin authorizes)
                                                     |
                                                     v
                                                Authorized
                                                     |
                                              (admin applies)
                                                     |
                                                     v
                                              AppliedCanary
                                                   / \
                                                  /   \
                                                 v     v
                                           Promoted  RolledBack
```

All state transitions are recorded in the proposal's `lifecycle: Vec<ProposalState>` for full auditability. Timestamps are updated on every transition via `lifecycle::transition()`.

### 3.2 Proposal Data Model

```rust
struct ChangeProposal {
    proposal_id: String,          // UUID v4
    proposal_hash: String,        // Deterministic hash of (kind, target_id, payload)
    kind: ChangeProposalKind,     // ConfigChange | SkillAdd | SkillUpdate | AgentAdd | AgentUpdate
    target_id: Option<String>,    // skill_id or agent_id (None for ConfigChange)
    payload: serde_json::Value,   // Kind-specific mutation data
    created_at: String,           // RFC 3339
    updated_at: String,           // RFC 3339, updated on every transition
    state: ProposalState,         // Current lifecycle state
    lifecycle: Vec<ProposalState>,// Full state history
    verification_report: VerificationReport,
    audit_verdict: AuditVerdict,
    authorization: ProposalAuthorization,
    apply_result: Option<ProposalApplyResult>,
}
```

### 3.3 Proposal Kinds

| Kind | target_id | Payload Shape | Description |
|------|-----------|---------------|-------------|
| `ConfigChange` | None | `{ patch, simulate_regression?, required_safeguards? }` | TOML config overlay |
| `SkillAdd` | `skill_id` | `{ content, simulate_regression?, required_safeguards? }` | New skill definition |
| `SkillUpdate` | `skill_id` | `{ patch, simulate_regression?, required_safeguards? }` | Update existing skill |
| `AgentAdd` | `agent_id` | `{ patch, simulate_regression?, required_safeguards? }` | New agent definition |
| `AgentUpdate` | `agent_id` | `{ patch, simulate_regression?, required_safeguards? }` | Update existing agent |

### 3.4 Deterministic Hashing

`lifecycle::proposal_hash` produces a 16-character hex hash from `(kind_debug_str, target_id, payload_json)` using `DefaultHasher`. This hash is recorded in audit records and used for proposal identity verification across the lifecycle.

---

## 4. Verification

Verification runs synchronously during proposal creation (`state::create_proposal`) and produces a `VerificationReport`:

```rust
struct VerificationReport {
    passed: bool,
    issues: Vec<String>,
    blocked_by_guard: bool,
}
```

### 4.1 Verification Checks by Kind

| Kind | Checks |
|------|--------|
| `ConfigChange` | Patch string must not be empty |
| `SkillAdd` | Skill must not already exist; content must not be empty; `SkillLinter` must not produce `Error`-severity warnings |
| `SkillUpdate` | Skill must exist (in `known_skill_ids`, `managed_skills`, or agent capabilities); patch must not be empty; lint check |
| `AgentAdd` | Agent must not already exist (in subagents or managed agents); patch must not be empty; all skills referenced in capabilities must be known |
| `AgentUpdate` | Agent must exist; patch must not be empty; all skills referenced in capabilities must be known |

### 4.2 Agent Patch Skill Validation

`verification::validate_agent_patch_skills` parses the patch as JSON, extracts `capabilities.skills`, and rejects any skill ID not found in the known skills set. This prevents agents from being configured with non-existent skills -- a "central config scope violation."

### 4.3 Known Skills Resolution

`verification::known_skills` computes the union of:
1. `known_skill_ids` (loaded from the skill registry at startup)
2. `managed_skills` (skills added via proposals at runtime)
3. Skills referenced in any agent's `capabilities.skills` in the config snapshot

---

## 5. Audit

Audit runs immediately after verification during proposal creation and produces an `AuditVerdict`:

```rust
struct AuditVerdict {
    risk_level: AuditRiskLevel,    // Low | Medium | High | Critical
    exploitability_notes: Vec<String>,
    allow: bool,
    required_safeguards: Vec<String>,
}
```

### 5.1 Risk Scoring Algorithm

1. **Base risk: Low** for all proposals.
2. **Verification failure** escalates to **High** and adds an exploitability note.
3. **Critical exploit markers** in the payload text escalate to **Critical**:
   - `rm -rf /`
   - `curl ` (arbitrary download)
   - `| sh`, `| bash` (pipe to shell)
   - `ignore previous instructions` (prompt injection)
   - `disregard your system prompt` (prompt injection)
4. `ConfigChange` and `AgentUpdate` kinds default to **Medium** if no higher risk was detected.
5. **High/Critical** proposals automatically require `human_approval` safeguard.

### 5.2 Allow/Block Decision

```
allow = verification.passed AND risk_level NOT IN (High, Critical)
```

Blocked proposals transition to `AuditBlocked` and cannot proceed without being recreated.

### 5.3 Mutation Audit Records

Every mutation path (authorize, apply, modify_skill) records a `MutationAuditRecord`:

```rust
struct MutationAuditRecord {
    at: String,                          // RFC 3339 timestamp
    trigger_type: TriggerType,           // Admin | User | Internal
    proposal_id: Option<String>,
    proposal_hash: Option<String>,
    action: String,                      // e.g., "authorize_proposal", "apply_authorized_proposal"
    outcome: String,                     // e.g., "authorized", "denied_non_admin_trigger", "promoted", "rolled_back"
    details: serde_json::Value,          // Action-specific context
}
```

Recorded outcomes include:
- `denied_non_admin_trigger` -- non-admin attempted a mutation
- `denied_verification_failed` -- verify_before_authorize blocked
- `denied_high_risk_without_force` -- high/critical risk without `force=true`
- `denied_not_authorized` -- apply attempted before authorization
- `authorized` -- proposal authorized
- `promoted` -- mutation applied and promoted
- `rolled_back` -- mutation applied but reverted (guard block, canary failure, apply error)
- `rolled_back_guard_block` -- execution guard blocked apply

---

## 6. Security Model

### 6.1 Trigger Gate

`security::trigger_gate::ensure_admin_trigger` is the single enforcement point for privilege checks:

```rust
fn ensure_admin_trigger(
    trigger_type: TriggerType,
    require_admin: bool,
    action: &str,
) -> Result<(), String>
```

When `require_admin` is `true` (controlled by `self_improvement.require_admin_message_for_mutations`), any non-`Admin` trigger type produces an error string that is returned as a `ToolOutput::Error` and logged as a `denied_non_admin_trigger` audit record.

This gate is called by:
- `authorize_proposal`
- `apply_authorized_proposal`
- `modify_skill`

### 6.2 Privilege Boundary

In v0.1-alpha, the privilege boundary is `TriggerType`, not transport-level authentication:

| Ingress | TriggerType | Can Mutate? |
|---------|-------------|-------------|
| CLI (`neuroctl orchestrator turn`) | `Admin` | Yes |
| CLI (`neuroctl orchestrator chat`) | `Admin` | Yes |
| Discord gateway | `User` | No |
| Cron scheduler | `Internal` | No |
| Internal delegation | `Internal` | No |

No transport authentication mechanism is implemented. The admin API binds to `127.0.0.1:9090` by default, relying on localhost-only access as the perimeter.

### 6.3 High-Risk Proposal Gating

When `authorize_proposal` encounters a proposal with `AuditRiskLevel::High` or `AuditRiskLevel::Critical`, it requires `force=true` in the tool call arguments. Without it, the authorization is denied with outcome `denied_high_risk_without_force`. This forces the admin (or System0 acting on admin instruction) to explicitly acknowledge the risk.

### 6.4 Verification-Before-Authorize Gate

When `self_improvement.verify_before_authorize` is `true` (default), `authorize_proposal` rejects proposals where `verification_report.passed == false`. This prevents authorizing proposals that failed structural validation. The gate can be disabled for testing or when accepting known-imperfect proposals.

### 6.5 Execution Guard

The `ExecutionGuard` trait provides three fail-closed hooks:

```rust
trait ExecutionGuard: Send + Sync {
    fn pre_verify_proposal(&self, proposal: &ChangeProposal) -> Result<(), OrchestratorRuntimeError>;
    fn pre_apply_proposal(&self, proposal: &ChangeProposal) -> Result<(), OrchestratorRuntimeError>;
    fn pre_skill_script_execution(&self, skill_id: &str, required_safeguards: &[String]) -> Result<(), OrchestratorRuntimeError>;
}
```

The current implementation (`PlaceholderExecutionGuard`) checks `required_safeguards` for any entry containing `"sandbox"` or `"container_required"` (case-insensitive). If found, it returns `GuardBlocked("blocked_missing_sandbox")`.

**Guard integration points:**

| Hook | Called By | Effect on Failure |
|------|-----------|-------------------|
| `pre_verify_proposal` | `create_proposal` | Overwrites verification to failed, sets `blocked_by_guard = true`, escalates audit to `Critical`, blocks proposal |
| `pre_apply_proposal` | `apply_authorized_proposal`, `modify_skill` | Immediate rollback, proposal transitions to `RolledBack` |
| `pre_skill_script_execution` | `SkillToolBroker::call_tool` | Skill invocation fails with `ToolError::ExecutionFailed` |

### 6.6 Event Payload Redaction

`security::redaction` provides two protections for event payloads stored in the thread journal:

1. **Secret masking** -- Collects environment variable values where the key contains `KEY`, `TOKEN`, `SECRET`, or `PASSWORD`, then replaces occurrences in all string values with `[REDACTED]`. Object keys matching the sensitive pattern are replaced entirely.

2. **Payload shape restriction** -- For `tool_call` and `tool_result` event types, only whitelisted top-level keys are allowed: `call_id`, `tool_id`, `arguments`, `status`, `output`. Events with unexpected keys are silently dropped (payload set to `null`).

---

## 7. Tool Class Hierarchy

### 7.1 Classification

`dispatch::classify_tool` maps tool IDs to one of four classes:

```rust
enum ToolClass {
    Runtime,                  // Always available
    Adaptive,                 // Available when self-improvement enabled
    AuthenticatedAdaptive,    // Requires Admin trigger + self-improvement enabled
    Unknown,                  // Not recognized
}
```

### 7.2 Tool Inventory

#### Runtime Tools (3)

| Tool | Arguments | Description |
|------|-----------|-------------|
| `delegate_to_agent` | `{ agent_id, instruction }` | Delegate instruction to a sub-agent |
| `list_agents` | `{}` | List configured sub-agent IDs |
| `read_config` | `{}` | Read orchestrator config snapshot |

#### Adaptive Tools (13)

| Tool | Arguments | Description |
|------|-----------|-------------|
| `list_proposals` | `{}` | List all proposals with ID, hash, kind, state, timestamps |
| `get_proposal` | `{ proposal_id }` | Get full proposal detail |
| `propose_config_change` | `{ patch, simulate_regression?, required_safeguards? }` | Create config change proposal |
| `propose_skill_add` | `{ skill_id, content, simulate_regression?, required_safeguards? }` | Create skill add proposal |
| `propose_skill_update` | `{ skill_id, patch, simulate_regression?, required_safeguards? }` | Create skill update proposal |
| `propose_agent_add` | `{ agent_id, patch, simulate_regression?, required_safeguards? }` | Create agent add proposal |
| `propose_agent_update` | `{ agent_id, patch, simulate_regression?, required_safeguards? }` | Create agent update proposal |
| `analyze_failures` | `{}` | Cluster failed delegated runs by agent and error signature |
| `score_skills` | `{}` | Score skill quality (success rate, stale detection) |
| `adapt_routing` | `{}` | Recommend policy-bounded routing weight updates |
| `record_lesson` | `{ lesson }` | Store a lesson-learned in the `system:lessons` partition |
| `run_redteam_eval` | `{}` | Run lightweight red-team checks (prompt injection surface, skill exfiltration, unauthorized mutation) |
| `list_audit_records` | `{}` | List all mutation audit records |

#### Authenticated Adaptive Tools (3)

| Tool | Arguments | Description |
|------|-----------|-------------|
| `authorize_proposal` | `{ proposal_id, force? }` | Authorize a verified/audited proposal (admin trigger required) |
| `apply_authorized_proposal` | `{ proposal_id }` | Apply an authorized proposal with canary + rollback (admin trigger required) |
| `modify_skill` | `{ skill_id, patch }` | Shortcut: create + verify + audit + authorize + apply a skill mutation in one step (admin trigger required) |

### 7.3 Tool Visibility

`System0ToolBroker::visible_tool_specs` filters the full tool spec list by:

1. The tool must be in `allowlisted_tools` (from config or defaults)
2. The tool must be in the agent context's `allowed_tools`
3. If self-improvement is disabled, adaptive and authenticated-adaptive tools are hidden

When self-improvement is disabled, all adaptive/authenticated-adaptive tool calls return `ToolOutput::Error("self-improvement is disabled")`.

---

## 8. Adaptation Analytics

### 8.1 Failure Clustering

`adaptation::analytics::failure_clusters` aggregates failed delegated runs from `runs_index` into two dimensions:

- **By agent**: counts of failed runs per `agent_id`
- **By signature**: counts of failed runs per error signature (first word of the run summary, lowercased)

Output:
```json
{
  "by_agent": { "planner": 3, "researcher": 1 },
  "by_signature": { "timeout": 2, "connection": 1, "unknown": 1 }
}
```

### 8.2 Skill Quality Scoring

`adaptation::analytics::skill_scores` computes per-skill quality metrics:

```
quality_score = 1.0 - (failures / invocations)
stale_candidate = (invocations == 0)
```

Skills with zero invocations receive a score of 1.0 but are flagged as stale candidates. The output is sorted by skill ID for deterministic ordering.

Skill quality stats are updated during proposal creation for skill-related proposals: invocations are incremented, and failures are incremented if the proposal fails verification or audit.

### 8.3 Routing Adaptation

`adaptation::analytics::routing_adaptation` computes per-agent success rates from `runs_index`:

```
success_rate = completed_runs / total_runs
recommended_weight = round(success_rate * 100) / 100
```

All recommendations are annotated with `bounded_by_policy: true` to indicate that actual routing changes must flow through the proposal lifecycle.

### 8.4 Lessons Learned

The `record_lesson` tool appends free-text lessons to `System0BrokerInner.lessons_learned` (in-memory `Vec<String>`). Lessons are partitioned under `system:lessons` for future persistence via `MemoryStore`.

### 8.5 Red-Team Evaluation

`adaptation::redteam::run_redteam_eval` runs three lightweight security checks:

1. **Prompt injection surface** -- Checks for known injection patterns
2. **Skill exfiltration patterns** -- Checks for data leak vectors in skills
3. **Unauthorized mutation attempts** -- Reviews audit log for non-admin mutation attempts

The current implementation returns a static pass for all three checks, acting as a scaffold for future continuous security evaluation.

---

## 9. Canary Testing and Rollback

### 9.1 Canary Metrics

`adaptation::canary::canary_metrics` collects runtime health metrics:

```rust
struct CanaryMetrics {
    total_runs: usize,
    failed_runs: usize,
    success_rate_pct: f64,
    tool_failure_rate_pct: f64,
    policy_denial_rate_pct: f64,  // Currently always 0.0 (not yet tracked)
}
```

Metrics are computed from `runs_index` (delegated run outcomes) and `skill_quality_stats` (tool invocation/failure counts).

### 9.2 Rollback Decision

`adaptation::canary::rollback_reason` compares before/after metrics against configurable thresholds:

| Metric | Default Threshold | Trigger |
|--------|-------------------|---------|
| Success rate drop | 3.0% | `before.success_rate_pct - after.success_rate_pct > threshold` |
| Tool failure increase | 2.0% | `after.tool_failure_rate_pct - before.tool_failure_rate_pct > threshold` |
| Policy denial increase | 5.0% | `after.policy_denial_rate_pct - before.policy_denial_rate_pct > threshold` |

If any threshold is exceeded, a rollback reason string is returned and the mutation is reverted.

A `simulate_regression` flag in the proposal payload forces a rollback for testing purposes.

### 9.3 Rollback Mechanics

When `apply_authorized_proposal` detects a rollback trigger:

1. **State snapshot** -- Before applying, snapshots of `managed_skills`, `managed_agents`, `known_skill_ids`, `config_snapshot`, and `config_patch_history` are captured.
2. **Apply mutation** -- The proposal mutation is applied to the live state.
3. **Canary check** -- If `canary_before_promote` is `true`, before/after metrics are compared.
4. **Rollback** -- If a rollback reason exists (apply error, canary regression, or simulated regression):
   - All five state snapshots are restored
   - `apply_result` is set to `{ promoted: false, rolled_back: true, reason }`
   - Proposal transitions to `RolledBack`
   - Audit record logged with outcome `rolled_back`
5. **Promotion** -- If no rollback reason:
   - `apply_result` is set to `{ promoted: true, rolled_back: false }`
   - Proposal transitions to `Promoted`
   - `last_known_good_snapshot` is updated
   - Audit record logged with outcome `promoted` including before/after metrics

---

## 10. Skill Management

### 10.1 SKILL.md Format

Skills are defined as directories containing a `SKILL.md` file with YAML frontmatter:

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
      mode: "isolated"
      network: "egress-web"
      script: "scripts/run.py"
      timeout_ms: 3000
    models:
      preferred: "browser_model"
    data_sources:
      markdown: ["data/bills.md"]
      csv: ["data/accounts.csv"]
    safeguards:
      human_approval:
        - "purchase"
        - "file_write_outside_workspace"
---
# Browser Summarize

Instructions for the agent...
```

### 10.2 Skill Registry

`SkillRegistry` supports progressive loading:

1. **Scan phase** (`registry.scan()`) -- Reads all `SKILL.md` files in configured directories, parses frontmatter, collects resource files. Instructions body is **not** loaded.
2. **Load phase** (`registry.load_instructions(name)`) -- Loads the full instructions body and runs the linter.

This two-phase approach allows fast startup with deferred full loading.

### 10.3 Skill Linter

`SkillLinter` scans instruction text line-by-line against 18 built-in dangerous patterns:

| Category | Patterns | Severity |
|----------|----------|----------|
| Secret exfiltration | `print your api key`, `print your password`, `print your secret`, `send your credentials`, `include the api key in`, `base64 encode the secret` | Error |
| Plaintext storage | `store password in`, `store api key in`, `store secret in` | Error |
| Shell injection | `\| sh`, `\| bash`, `rm -rf /` | Error |
| Arbitrary execution | `download and execute`, `download and run` | Error |
| Prompt injection | `ignore previous instructions`, `disregard your system prompt` | Error |
| Dynamic code | `eval(`, `exec(` | Warning |

Custom patterns can be added via `linter.add_pattern()`. Case-insensitive matching is used for all patterns except shell-specific ones.

The linter is invoked during:
- Skill instruction loading (`SkillRegistry::load_instructions`)
- Proposal verification for `SkillAdd` and `SkillUpdate` kinds

### 10.4 Skill Tool Broker

`SkillToolBroker` implements `ToolBroker` and handles skill tool invocations:

1. **Tool resolution** -- Maps requested tool ID to canonical skill name via `tool_aliases`
2. **Markdown loading** -- Reads markdown data sources from `local_root` with path policy enforcement
3. **CSV loading** -- Reads and parses CSV data sources with column headers, row data, and summary stats
4. **Script execution** -- If the skill defines an `execution.script`, runs it through `script_runner`
5. **Output assembly** -- Returns `{ skill, markdown, csv, script_result }`

### 10.5 Script Runner

`skills::script_runner::run_skill_script` executes skill scripts as isolated Python processes:

- **Invocation**: `python3 -I <script_path>` (`-I` flag for isolated mode)
- **Input**: JSON payload on stdin containing `{ local_root, skill, data_sources, arguments }`
- **Output**: JSON on stdout (parsed and returned)
- **Timeout**: Configurable per-skill, default 5 seconds
- **Failure modes**: IO errors, non-zero exit, invalid JSON output, timeout (kills process)

The execution guard's `pre_skill_script_execution` hook runs before the script process is spawned.

### 10.6 Path Policy

`skills::path_policy` enforces filesystem containment:

- Rejects absolute paths
- Rejects parent directory traversal (`..`, root, prefix components)
- Canonicalizes both root and target paths
- Verifies the resolved path starts with the canonicalized root

Applied to both data source paths (relative to `local_root`) and script paths (relative to `skill_root`).

---

## 11. Managed Runtime State

All self-adaptive state lives in `System0BrokerInner`, protected by `Arc<AsyncMutex<...>>`:

```rust
struct System0BrokerInner {
    // Self-improvement config
    self_improvement: SelfImprovementConfig,
    execution_guard: Arc<dyn ExecutionGuard>,

    // Proposal state
    proposals_index: HashMap<String, ChangeProposal>,     // proposal_id -> proposal
    proposals_order: Vec<String>,                          // insertion-ordered proposal IDs

    // Managed mutations (applied proposals)
    managed_skills: HashMap<String, String>,               // skill_id -> content/patch
    managed_agents: HashMap<String, serde_json::Value>,    // agent_id -> config JSON
    config_patch_history: Vec<String>,                     // ordered config patches
    config_snapshot: serde_json::Value,                    // current effective config
    last_known_good_snapshot: serde_json::Value,           // last promoted config

    // Quality tracking
    skill_quality_stats: HashMap<String, SkillQualityStats>,  // { invocations, failures }
    known_skill_ids: HashSet<String>,                         // all known skill IDs
    lessons_learned: Vec<String>,                             // recorded lessons

    // Audit
    mutation_audit_log: Vec<MutationAuditRecord>,

    // Trigger context (set per turn)
    current_trigger_type: TriggerType,
    current_turn_id: uuid::Uuid,

    // ... (runtime state: subagents, runs, threads, etc.)
}
```

### 11.1 State Durability

In v0.1-alpha, all self-adaptive state is **in-memory only**. State is lost on daemon restart. Future versions will persist proposals, audit logs, and managed mutations to the SQLite-backed `MemoryStore`.

### 11.2 Config Snapshot Management

- On startup, `config_snapshot` is initialized from the serialized `NeuromancerConfig`
- `last_known_good_snapshot` is set to the same initial value
- On promotion, `last_known_good_snapshot` is updated to the current `config_snapshot`
- On rollback, `config_snapshot` is restored from the pre-apply snapshot (not `last_known_good_snapshot`)

---

## 12. `modify_skill` Compatibility Path

`modify_skill` is a convenience alias that collapses the full proposal lifecycle into a single admin tool call. It does **not** bypass any security checks:

```
modify_skill(skill_id, patch)
  1. ensure_admin_trigger
  2. Determine kind: SkillUpdate if skill exists, SkillAdd otherwise
  3. create_proposal (verify + audit + guard)
  4. If verification failed or audit blocked -> return { status: "blocked" }
  5. Authorize (inline, with force=false)
  6. pre_apply_proposal guard check
  7. Apply mutation
  8. On error -> rollback, return { status: "rolled_back" }
  9. On success -> promote, audit log, return { status: "accepted" }
```

The key difference from the two-step `authorize_proposal` + `apply_authorized_proposal` flow:
- `modify_skill` does **not** run canary regression checks (no before/after metric comparison)
- `modify_skill` always uses `force=false`, meaning high/critical-risk skills that pass audit will still be applied (since audit already passed, the force gate is in `authorize_proposal` not `modify_skill`)
- The proposal is still fully recorded in `proposals_index` and `proposals_order`

---

## 13. Configuration

### 13.1 TOML Surface

```toml
[orchestrator.self_improvement]
enabled = true                              # Master toggle (default: false)
audit_agent_id = "audit-agent"              # Required agent (default: "audit-agent")
require_admin_message_for_mutations = true  # Admin gate (default: true)
verify_before_authorize = true              # Verification prerequisite (default: true)
canary_before_promote = true                # Canary regression check (default: true)

[orchestrator.self_improvement.thresholds]
max_success_rate_drop_pct = 3.0             # Max allowed success rate decrease
max_tool_failure_increase_pct = 2.0         # Max allowed tool failure increase
max_policy_denial_increase_pct = 5.0        # Max allowed policy denial increase
```

### 13.2 Validation Rules

At startup and on config hot-reload (`neuromancerd/src/config.rs`):

1. If `self_improvement.enabled == true`, the `audit_agent_id` must exist as a key in `[agents]`. Config validation fails (bail) if the agent is missing.
2. All other standard validations apply (model slot references, MCP server references, A2A peer references, prompt file existence).

### 13.3 Default Tool Allowlist

When `orchestrator.capabilities.tools` is empty, all 19 tools are allowlisted by default (3 runtime + 13 adaptive + 3 authenticated adaptive). The allowlist can be restricted by explicitly configuring `capabilities.tools` in the orchestrator config.

---

## 14. Testing Surface

`neuromancerd/src/orchestrator/runtime_tests.rs` covers:

| Test | Behavior |
|------|----------|
| Non-admin denial for `authorize_proposal` | `TriggerType::User` returns error, audit logged as `denied_non_admin_trigger` |
| Non-admin denial for `apply_authorized_proposal` | Same as above |
| Admin authorize happy path | Proposal transitions to `Authorized`, audit logged |
| Admin apply happy path | Proposal transitions to `Promoted`, `last_known_good_snapshot` updated |
| Dangerous proposal blocking | High/critical risk proposals blocked without `force=true` |
| Unknown skill rejection in agent updates | Agent patches referencing unknown skills fail verification |
| Canary rollback | `simulate_regression=true` forces rollback; state restored |
| Mutation audit logging | All paths produce audit records with correct outcomes |
| `modify_skill` compatibility | Full lifecycle in single call, audit logged |
| Self-improvement disabled | All adaptive tools return error when `enabled=false` |

Additional test coverage exists in:
- `neuromancer-skills/src/linter.rs` -- Pattern detection (clean, API key, curl pipe, prompt injection, custom patterns, line numbers, multiple warnings)
- `neuromancer-skills/src/metadata.rs` -- Frontmatter parsing (full skill, minimal, missing name, no frontmatter)
- `neuromancer-skills/src/registry.rs` -- Directory scanning, progressive loading, agent filtering, lint-on-load
- `neuromancerd/src/orchestrator/skills/script_runner.rs` -- JSON output, timeout, invalid JSON rejection
- `neuromancerd/src/orchestrator/skills/path_policy.rs` -- Absolute path rejection, parent traversal rejection, valid relative paths
- `neuromancer-core/src/config.rs` -- Self-improvement defaults, custom value parsing

---

## 15. Future Work

### 15.1 Not Yet Implemented

- **Transport authentication** -- No auth mechanism on the admin API. `TriggerType` is the only privilege boundary.
- **Policy denial tracking** -- `policy_denial_rate_pct` in canary metrics is hardcoded to `0.0`.
- **Persistent proposals** -- Proposals, audit logs, and managed mutations are in-memory only.
- **Red-team evaluation** -- Current implementation returns static pass results; no real analysis.
- **Execution sandbox** -- `PlaceholderExecutionGuard` blocks operations requiring sandbox/container support rather than providing them.
- **Audit agent integration** -- The audit agent is required to exist in config but is not yet invoked during the audit phase.

### 15.2 Extension Points

- `ExecutionGuard` trait is designed for replacement with container/sandbox-aware implementations
- `SkillLinter` supports custom patterns via `add_pattern`
- `MutationAuditRecord.details` accepts arbitrary JSON for future structured data
- `lessons_learned` is designed for future persistence via `MemoryStore` with the `system:lessons` partition
- Config hot-reload re-validates self-improvement constraints on every reload
