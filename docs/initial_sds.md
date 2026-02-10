# Neuromancer v0.5 System Design Specification

## 0. Status and scope

**Status:** Draft proposal for v0.5 (“first daemon you can keep running without fear”).
**Scope:** A Rust daemon (“Neuromancer”) that orchestrates a core agent and multiple isolated sub‑agents, using:

* **Agent Skills** for instruction bundles
* **MCP** for external tools/services (via the official Rust SDK)
* **A2A** for agent-to-agent delegation and isolation boundaries
* **OpenTelemetry** for end-to-end observability

No new “plugin protocol” is introduced: external extensibility is primarily via **MCP servers** and **A2A peers**, with **skills** as the “instruction + packaging” layer.

---

## 1. Motivation and problem statement

Neuromancer is explicitly designed around failures we’ve now repeatedly seen in the wild:

* **Secrets leaking into model context and logs**: there are reports of resolved provider API keys being serialized into the LLM prompt context on every turn in OpenClaw gateway setups—meaning keys can leak cross-provider and persist in provider logs. ([GitHub][1])
* **“Leaky skills” as a systemic pattern**: security research found a non-trivial fraction of marketplace skills that *instruct the agent* to mishandle secrets/PII, forcing API keys/passwords/credit-card data into the LLM context window or plaintext logs—i.e., not “a bug,” but a predictable outcome of the architecture. ([Snyk][2])
* **Long-running instability and process leakage**: reports of orphaned child processes accumulating over 24–48 hours, memory pressure peaking in the tens of GB, and degraded Discord processing latency. ([GitHub][3])
* **Supply chain reality**: credible practitioners are now converging on the same missing ingredients—provenance, mediated execution, and **specific + revocable permissions** tied to identity. ([1Password][4])

**Neuromancer v0.5** is therefore defined by: **least privilege**, **isolation**, **brokered secrets**, and **observable operations**, while staying compatible with the ecosystems forming around MCP, A2A, and skills.

---

## 2. Goals and non-goals

### 2.1 Goals

1. **Daemon-first architecture**
   A resilient, long-running service with explicit lifecycle management and resource controls (no “CLI app that you keep alive with prayers”).

2. **Extensible minimal core**
   The core runtime can run an agent loop, call tools, execute skills, and delegate via A2A—everything else is modular.

3. **No new plugin protocol**

   * Tools/services → **MCP**
   * Remote agents → **A2A**
   * Instruction packaging → **Skills**
     The “tool surface” is unified internally, but external “plugins” are expected to be MCP servers or A2A agents.

4. **Security by construction**

   * Secrets never go into LLM context by default
   * Capabilities are explicit and auditable (skills, MCP servers, A2A peers, memory partitions, filesystem roots, network egress)
   * Sub-agents run isolated (prefer container), with a clear request path for out-of-bounds capabilities

5. **Full observability**

   * Structured traces/spans for: trigger → task → LLM calls → tool calls → secret accesses → memory reads/writes
   * Export via OpenTelemetry OTLP

### 2.2 Non-goals (v0.5)

* A public skill marketplace / package manager (v0.5 supports local skill packs; supply-chain hardening is *foundation work*).
* Full multi-tenant enterprise RBAC UI.
* A polished web UI (v0.5 can expose a local admin API, but not required).

---

## 3. High-level architecture

Neuromancer is split into **control plane** and **data plane**.

### 3.1 Control plane: `neuromancerd` (daemon)

Responsibilities:

* Load **central TOML config**
* Run **Trigger Manager** (Discord + cron)
* Maintain **Secrets Broker**
* Maintain **Memory Store**
* Maintain **MCP Registry** (server configs + allowlists)
* Maintain **A2A Registry** (peer configs + allowlists)
* **Supervise agent runtimes** (in-process, subprocess, or container)
* Provide a **task queue** and **audit trail**

### 3.2 Data plane: agent runtimes (“workers”)

Each agent runtime is an instance of the same core, configured with a strict **capability set**:

* Allowed skills
* Allowed MCP servers/tools
* Allowed A2A peers
* Allowed secrets (by handle)
* Allowed memory partitions
* Allowed filesystem roots
* Allowed outbound network policy

**Preferred pattern:** root orchestrator delegates work to specialized sub-agents (“browser”, “files”, “remote”), rather than giving one agent god-mode.

### 3.3 Diagram

```
                         +---------------------------+
Discord / Cron Triggers ->|   neuromancerd (control)  |
                         |  - config + policy         |
                         |  - secrets broker          |
                         |  - memory store            |
                         |  - OTEL exporter           |
                         |  - agent supervisor        |
                         +-----------+----------------+
                                     |
                                     | A2A (HTTP+JSON/SSE)
                                     v
     +-------------------+     +-------------------+     +-------------------+
     | Agent: core       |     | Agent: browser    |     | Agent: files      |
     | (planner/router)  |     | (isolated)        |     | (isolated)        |
     | allowed: minimal  |     | allowed: browser  |     | allowed: fs tools |
     +---------+---------+     +---------+---------+     +---------+---------+
               |                         |                         |
               | MCP (rmcp)              | MCP                      | MCP
               v                         v                         v
        [MCP servers...]         [Playwright MCP]         [FS MCP / built-in]
```

---

## 4. Protocol choices (why “no new plugin protocol” works)

### 4.1 MCP for tools

Neuromancer integrates tools via **MCP**, using the **official Rust SDK** (`rmcp`) which provides protocol implementation and supports building clients/servers on Tokio. ([GitHub][5])
This is the primary path for:

* Browser automation servers
* File tooling servers
* External SaaS/API connectors
* “Sidecar” services (indexers, search, etc.)

### 4.2 A2A for delegation and isolation

Neuromancer uses **A2A** as the standard for sub-agent delegation and isolation boundaries.

Key A2A elements Neuromancer relies on:

* **Agent discovery via Agent Card** at `/.well-known/agent-card.json` ([A2A Protocol][6])
* **HTTP+JSON binding** endpoints like `POST /message:send`, `POST /message:stream` (SSE), `GET /tasks/{id}`, etc. ([A2A Protocol][6])
* Content type `application/a2a+json` and versioning headers ([A2A Protocol][6])
* Strong guidance on permission failures and non-leaky errors (helpful to model Neuromancer’s “capability missing” UX) ([A2A Protocol][6])
* Agent Cards may be signed with JWS; clients should verify when present ([A2A Protocol][6])

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
  `neuromancer-core` library crate: runtime, tool broker, memory interface, policy hooks.
* **Detailed logging via OpenTelemetry**
  `tracing` + OTLP exporter; spans for every step; trace propagation across A2A and MCP.
* **Core agent can run skills, use MCP, and A2A**
  Tool broker with 3 backends: SkillsExecutor, McpClientPool, A2aClient.
* **Centralized configuration format (TOML)**
  Single TOML file defines org-tree of agents, triggers, servers, secrets, memory partitions.
* **Trigger system to core agent**
  Trigger Manager produces tasks into queue → core orchestrator decides routing.
* **Triggers supported: Discord and cron**
  Discord via `twilight` or `serenity`; cron via `tokio-cron-scheduler`.
* **Secure secrets store with access controls**
  Secrets Broker with encrypted-at-rest store, ACLs, and “handle-based injection”.
* **Hierarchical, partitioned memory**
  Memory partitions per agent/scope with explicit sharing rules.

---

## 6. Core runtime design (`neuromancer-core`)

### 6.1 Core interfaces (traits)

This is the key to modularity and unit-testability.

```rust
trait LlmProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
}

trait ToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec>;
    async fn call_tool(&self, ctx: &AgentContext, call: ToolCall) -> ToolResult;
}

trait MemoryStore {
    async fn put(&self, ctx: &AgentContext, item: MemoryItem) -> Result<()>;
    async fn query(&self, ctx: &AgentContext, q: MemoryQuery) -> Result<Vec<MemoryItem>>;
}

trait SecretsBroker {
    async fn resolve_handle_for_tool(
        &self,
        ctx: &AgentContext,
        secret_ref: SecretRef,
        usage: SecretUsage,
    ) -> Result<ResolvedSecret>; // often injected, not returned as plaintext
}

trait PolicyEngine {
    async fn pre_tool_call(&self, ctx: &AgentContext, call: &ToolCall) -> PolicyDecision;
    async fn pre_llm_call(&self, ctx: &AgentContext, req: &CompletionRequest) -> PolicyDecision;
    async fn post_tool_call(&self, ctx: &AgentContext, result: &ToolResult) -> PolicyDecision;
}
```

### 6.2 Agent loop (state machine)

1. Receive `Task` (from trigger or A2A)
2. Assemble context:

   * relevant memory (partitioned)
   * skill metadata (not full instructions unless needed)
   * available tools (MCP + local + A2A)
3. Call LLM (planner/executor model chosen by router)
4. Parse tool calls (or explicit action plan)
5. For each tool call:

   * policy check
   * secrets broker resolves secret handles/injection
   * execute tool (MCP / skill / A2A)
   * log spans + store memory artifacts
6. Produce final output + persist summary/episodic memory

This is explicitly designed to prevent “secret values in prompt” and to keep tool execution brokered.

---

## 7. LLM integration and model differentiation

### 7.1 Rig as an optional integration layer

Rig is a Rust library focused on ergonomics/modularity for LLM-powered apps; it provides abstractions for completion and embedding models and higher-level “Agent” constructs. ([Docs.rs][9])

**Recommendation for v0.5:**

* Use a Neuromancer-native `LlmProvider` trait.
* Provide a `rig-adapter` crate so Rig can be used as one backend (especially for multi-provider support and embeddings), without locking core architecture to Rig’s agent abstractions.

This keeps the daemon architecture independent while still leveraging Rig where it fits (providers, embeddings, vector store glue).

### 7.2 “Dedicated models” support (first-class)

Neuromancer includes a `ModelRouter`:

* `planner_model` (cheap reasoning/planning)
* `executor_model` (tool-use heavy)
* `browser_model` (text-only controller for browser agent)
* `verifier_model` (safety/guard checks)

The router can be driven by:

* central TOML config defaults
* per-agent overrides
* per-skill hinting in frontmatter (see §8)

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

**Important:** skill metadata is not sufficient to grant permissions. It’s a *declaration*. The central config must authorize it.

### 8.3 Parsing frontmatter (Rust crate reality)

Many skill ecosystems use YAML frontmatter. But `serde_yaml` is marked “no longer maintained” in its docs. ([Docs.rs][10])

For v0.5, implement YAML parsing using maintained components:

* `yaml-rust2` as a YAML 1.2 implementation ([Docs.rs][11])
* optionally `serde_yaml2` for serde integration (if you want typed structs)
* `gray_matter` as a convenient frontmatter extractor (supports YAML/TOML/JSON) ([Docs.rs][12])

### 8.4 Skill execution modes

Each skill can be executed as:

* **Host execution**: only when explicitly allowed (dangerous by default)
* **Isolated execution**: run in a sandbox (container) with minimal mounts and no ambient secrets

This directly responds to the demonstrated “skills as supply chain” risk, where provenance + mediated execution + specific permissions are repeatedly cited as the missing layer. ([1Password][4])

---

## 9. MCP integration (tools/services)

### 9.1 SDK choice

Use **RMCP** (`rmcp`) from the official MCP Rust SDK repository; it’s explicitly described as the official Rust SDK and supports Tokio. ([GitHub][5])

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

A2A spec guidance around permissions and non-leaky error behavior is adopted (e.g., don’t reveal resource existence to unauthorized clients). ([A2A Protocol][6])

### 10.3 “Out-of-bounds permission request” workflow

1. Sub-agent fails policy check (e.g., needs a secret, MCP server, filesystem root)
2. Sub-agent returns a structured “capability_missing” error payload
3. Orchestrator:

   * logs it
   * asks user via the trigger channel (Discord DM or configured approver)
   * if approved: issues a **one-time grant** scoped to *task_id* (or writes config update if configured)

---

## 11. Central TOML configuration (org-tree)

Use a single TOML file as the source of truth.

### 11.1 Design principles

* **Explicit capabilities**, no ambient permissions
* **Hierarchical inheritance** (org-tree):

  * children inherit parent defaults
  * can only reduce permissions unless explicitly allowed to expand (config flag)
* Every capability has:

  * a stable ID
  * audit metadata (owner, purpose)
  * scope (agent, skill, task)

### 11.2 Example config sketch

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

[agents.core]
mode = "inproc"
models.planner = "gpt-planner"
models.executor = "gpt-executor"
capabilities.skills = ["router", "task_manager"]
capabilities.mcp_servers = []
capabilities.a2a_peers = ["browser", "files"]
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:core"]

[agents.browser]
mode = "container"
image = "neuromancer:0.5"
capabilities.skills = ["browser_summarize", "browser_clickpath"]
capabilities.mcp_servers = ["playwright"]
capabilities.secrets = ["web_login_cookiejar"]
capabilities.memory_partitions = ["workspace:default", "agent:browser"]

[agents.files]
mode = "container"
image = "neuromancer:0.5"
capabilities.skills = ["fs_readwrite"]
capabilities.mcp_servers = ["filesystem"]
capabilities.secrets = []
capabilities.memory_partitions = ["workspace:default", "agent:files"]

[triggers.discord]
enabled = true
token_secret = "discord_bot_token"
allowed_guilds = ["123456"]
route_agent = "core"

[[triggers.cron]]
schedule = "0 */2 * * * *"
route_agent = "core"
payload = "Summarize new issues and report"
```

### 11.3 Hot reload

Add a file watcher to reload config safely (with validation + diff). `notify` provides cross-platform filesystem notifications. ([Docs.rs][13])

---

## 12. Secrets store and access controls

### 12.1 Core rule

**Secrets are not LLM context.**
Agents reference secrets by **handle**; only tool execution contexts receive secret values (ideally injected into a subprocess/container environment).

This is a direct architectural countermeasure to the class of “keys serialized into prompt context” bugs. ([GitHub][1])

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

  * either via Rig’s embeddings abstractions ([Docs.rs][9])
  * or external vector DB as MCP service

### 13.3 Memory record schema (conceptual)

* `memory_id`
* `partition`
* `kind`: {message, summary, fact, artifact, link, tool_result}
* `content` (text or structured JSON)
* `tags`
* `created_at`, `expires_at`
* `source`: {discord, cron, a2a, tool}
* `sensitivity`: {public, private, secret_ref_only}
* optional `embedding_ref`

### 13.4 “Never store raw secrets” rule

Memory may store:

* secret handles and references
* redacted snippets
  But never plaintext secrets (enforced by policy + linter + runtime filters).

---

## 14. Trigger system (Discord + cron)

### 14.1 Trigger manager contract

A trigger source produces:

```rust
struct TriggerEvent {
  trigger_id: String,
  occurred_at: DateTime,
  principal: Principal, // discord user, system cron, etc.
  payload: TriggerPayload, // message text, attachments, schedule payload, etc.
  route_hint: Option<AgentId>,
}
```

### 14.2 Discord trigger (v0.5)

Crate choice options:

* `twilight`: described as a powerful, flexible, scalable ecosystem of Rust libraries for Discord. ([Docs.rs][18])
* `serenity`: widely used Rust Discord library; `poise` is a command framework built on top. ([Docs.rs][19])

**Recommendation:** start with `twilight` for gateway/event scale and keep an abstraction layer so you can swap.

### 14.3 Cron trigger (v0.5)

Use `tokio-cron-scheduler` (cron-like scheduling on Tokio, optional persistence). ([Docs.rs][20])

---

## 15. Observability via OpenTelemetry

### 15.1 Why OTLP

`opentelemetry-otlp` supports exporting telemetry data (logs/metrics/traces) to an OpenTelemetry Collector and common backends, via gRPC or HTTP. ([Docs.rs][21])

### 15.2 What we instrument (required)

Each task run emits spans:

* `trigger.receive`
* `task.create`
* `agent.dispatch`
* `agent.loop.step`
* `llm.call` (model id, token counts, latency)
* `tool.call` (tool id, MCP server, A2A peer, skill id)
* `secret.access` (handle id only, no value)
* `memory.query` / `memory.put`
* `policy.decision` (allow/deny + reason code)

### 15.3 Log redaction requirements

* Never log raw prompts containing secrets
* Redact configured patterns
* Emit “structured events” rather than dumping payloads

(These are must-haves to prevent the ecosystem-wide “keys in logs” class of failures.)

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

## 17. Security model (the “missing trust layer”)

This is the heart of the design.

### 17.1 Identity + least privilege

* Every agent has an identity (agent_id + credentials for A2A)
* Every external capability is explicit and revocable
* This directly matches the industry recommendation that permissions must be “specific, revocable, continuously enforced,” not granted once and forgotten. ([1Password][4])

### 17.2 Policy engine + verification middleware

Neuromancer supports stacked gates:

1. **Static gates** (config policy): allow/deny by capability.
2. **Heuristic gates** (cheap): detect dangerous patterns (rm -rf, curl | sh, etc.).
3. **Verifier gate** (optional): call a dedicated “safeguard model” for tool call approval.
4. **Human-in-the-loop**: for destructive/sensitive actions.

### 17.3 Skill linting (v0.5 foundation)

Given research showing that skills often instruct agents to pass secrets into prompts/logs ([Snyk][2]), Neuromancer adds:

* **Skill linter** at install/load time:

  * flags “print your API key” patterns
  * flags instructions that tell the agent to store secrets in memory/config
  * flags commands that download/execute remote scripts
* **Runtime outbound filter** (optional):

  * prevent echoing secret handles resolved into plaintext

---

## 18. Crate selection (researched shortlist)

### Daemon/runtime fundamentals

* **Async runtime:** `tokio` (ecosystem standard; required by rmcp usage patterns) ([GitHub][5])
* **HTTP server (A2A/admin):** `axum` (ergonomics + modularity; uses tower ecosystem). ([Docs.rs][23])
* **TLS:** `rustls` (secure defaults, no unsafe features by default). ([Docs.rs][24])

### Protocols

* **MCP:** `rmcp` official SDK ([GitHub][5])
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

* **Discord:** `twilight` or `serenity` ([Docs.rs][18])
* **Cron:** `tokio-cron-scheduler` ([Docs.rs][20])

### Skills parsing

* **Frontmatter extraction:** `gray_matter` ([Docs.rs][12])
* **YAML parsing:** `yaml-rust2` (serde_yaml is unmaintained) ([Docs.rs][11])

### Container orchestration

* **Docker:** `bollard` ([Docs.rs][22])

### LLM integration

* **Optional provider abstraction:** Rig ([Docs.rs][9])
* **Recommended architecture:** Neuromancer-native trait + Rig adapter

---

## 19. v0.5 implementation milestones

1. **Repo + crate layout**

   * `neuromancer-core` (library)
   * `neuromancerd` (daemon binary)
   * `neuromancer-a2a` (module/crate)
   * `neuromancer-mcp` (module/crate)
   * `neuromancer-skills` (module/crate)
   * `neuromancer-storage` (sqlite/sqlx)
2. **TOML config + validation + hot reload**
3. **OTEL tracing pipeline**
4. **Secrets broker MVP**

   * keyring master key
   * age-encrypted payload storage
   * ACL enforcement
5. **Memory store MVP**

   * partitioned sqlite schema
   * basic query (partition + tags + recency)
6. **MCP client manager using rmcp**
7. **A2A HTTP+JSON endpoints**
8. **Agent supervisor**

   * inproc workers
   * container workers (bollard)
9. **Triggers**

   * cron
   * discord
10. **Skill loader + permission declarations + linter MVP**

---

## 20. Open questions (intentionally deferred)

* How strongly do you want to enforce skill provenance in v0.5 (signatures, SBOM, pinned git SHAs)?
* Should “file tools” be a built-in tool surface (preferred for safety) or only via MCP servers (preferred for protocol purity)?
  (My bias: built-in restricted FS tool for v0.5, plus MCP support for external servers.)

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

