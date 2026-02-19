### Peers vs agents vs sub‑agents (OpenClaw’s “who/where/which brain” model)

OpenClaw has a clean separation that’s worth stealing for your own agent framework:

* **Peer = the external conversation endpoint** (a specific DM sender, or a specific group, or a specific channel/room).
* **Agent = an internal “brain”** (workspace + state dir + session store + model/tool policy).
* **Sub‑agent = an internal background run** (a new isolated session created on demand, usually via `sessions_spawn`).

That separation is what lets OpenClaw do “one gateway, many surfaces, many brains, plus parallel background work” without everything turning into spaghetti.

---

## 1) How peers are differentiated

### A. At routing time (which internal agent gets the message)

OpenClaw’s **multi‑agent routing** uses **bindings**. Each binding can match:

* `channel` (required)
* `accountId` (optional; supports multi-account connectors)
* `peer` (optional; the most specific match)
* plus channel-specific IDs like `guildId` (Discord) or `teamId` (Slack)

A **peer match** is explicitly typed as:

```json
peer: { kind: "direct" | "group" | "channel", id: "<provider-specific id>" }
```

…and peer matches win first. ([OpenClaw][1])

The deterministic priority order is (most specific first): peer → guildId → teamId → accountId → channel-wide → default agent. ([OpenClaw][1])

**Concrete examples from the docs:**

* Route one WhatsApp DM to a “Deep Work” agent by binding a *direct* peer id like `+15551234567`. ([OpenClaw][2])
* Route a specific WhatsApp group to a “family” agent by binding a *group* peer id (WhatsApp group JID-like id). ([OpenClaw][2])

So: **peer IDs are provider-specific**, but OpenClaw normalizes them into a common `{ kind, id }` shape for routing. ([OpenClaw][1])

### B. At session time (which conversation state is used)

Once an internal agent is chosen, OpenClaw chooses a **session key**. For DMs, this is controlled by `session.dmScope`:

* `main`: all DMs share one “main” session (single-user friendly)
* `per-peer`: per sender
* `per-channel-peer`: per channel + sender
* `per-account-channel-peer`: per account + channel + sender (multi-account friendly) ([OpenClaw][3])

The session key formats are explicitly documented (examples):

* `main`: `agent:<agentId>:<mainKey>`
* `per-channel-peer`: `agent:<agentId>:<channel>:dm:<peerId>`
* group chats: `agent:<agentId>:<channel>:group:<id>` (and rooms/channels similarly) ([OpenClaw][3])

**Identity unification across channels:** `session.identityLinks` can map multiple provider-prefixed peer IDs to a canonical identity so “the same human” shares one DM session across channels when you’re using per-peer-ish modes. ([OpenClaw][3])

### Practical takeaway

OpenClaw uses **two layers**:

1. **Routing layer:** `(channel, accountId, peer{kind,id}, guild/team)` → **agentId**
2. **Conversation layer:** `(agentId, chatType, dmScope…)` → **sessionKey**

That’s a great generic architecture because it avoids conflating “who messaged me” with “which brain should think” with “which memory thread should be used.”

---

## 2) How sub‑agents are differentiated

In OpenClaw, “sub‑agents” are not *new configured agents*; they’re **background runs spawned into their own isolated session**.

### What makes a run a “sub‑agent run”

When the main agent calls `sessions_spawn`, OpenClaw creates a new session key that looks like:

* `agent:<agentId>:subagent:<uuid>`

…and runs it on a dedicated **`subagent` queue lane** so it won’t block the main conversation. ([OpenClaw][4])

Key behavior points:

* **Non-blocking spawn:** tool returns immediately with `{ status: "accepted", runId, childSessionKey }`. ([OpenClaw][4])
* **Result announcement:** when the sub-agent finishes, OpenClaw posts a summary back into the requester’s chat. ([OpenClaw][4])
* **Auto-archive:** sub-agent sessions auto-archive after a default interval (60 minutes by default; configurable). ([OpenClaw][4])
* **No nested fan-out:** sub-agents cannot call `sessions_spawn` (prevents runaway trees). ([OpenClaw][5])

### Cross-agent spawning (sub-agent under another agentId)

By default, a sub-agent spawns under the **caller’s agentId**. To allow cross-agent spawning, OpenClaw uses an allowlist:

* `agents.list[].subagents.allowAgents` (or `["*"]`)

This gates whether the caller can set `agentId` in `sessions_spawn`. ([OpenClaw][5])

So there are *two* “multi” concepts:

* **Multiple configured agents** (multi-agent routing) = multiple isolated brains/workspaces. ([OpenClaw][2])
* **Sub-agents** = background sessions spawned (possibly under other brains, if allowlisted). ([OpenClaw][4])

---

## 3) How the startup wizard works (`openclaw onboard`)

OpenClaw’s “startup wizard” is a CLI wizard that’s designed like a **re-runnable state machine** that writes config + credentials + workspace structure.

### The wizard flow (local mode)

The docs enumerate the steps; the highlights:

1. **Existing config detection**

    * If `~/.openclaw/openclaw.json` exists: Keep / Modify / Reset
    * Re-running doesn’t wipe anything unless you choose Reset (or pass `--reset`)
    * If config is invalid/legacy, it stops and asks you to run `openclaw doctor`
    * Reset uses `trash` with scopes (config only; config+creds+sessions; full reset incl workspace) ([OpenClaw][6])

2. **Model/Auth**

    * Can reuse env vars (e.g. `OPENAI_API_KEY`) or prompt and store for daemon use
    * Supports multiple auth styles depending on provider (API keys, OAuth reuse, etc.) ([OpenClaw][6])

3. **Workspace**

    * Default `~/.openclaw/workspace` (configurable)
    * Seeds the workspace files “needed for the agent bootstrap ritual” (their phrase, not mine) ([OpenClaw][6])

4. **Gateway settings**

    * Port/bind/auth/tailscale exposure, with a bias toward keeping auth enabled ([OpenClaw][6])

5. **Channels**

    * Prompts you through configuring connectors (tokens/QR/etc.)
    * DM security defaults to pairing: unknown DM sends a code; approve via `openclaw pairing approve <channel> <code>` ([OpenClaw][6])

6. **Daemon install**

    * macOS: LaunchAgent
    * Linux/WSL2: systemd user unit (tries to enable linger) ([OpenClaw][6])

7. **Health check**

    * Starts gateway if needed; runs `openclaw health` ([OpenClaw][6])

8. **Skills install** (optional but recommended)

    * Selects node manager, installs dependencies, may use platform tools ([OpenClaw][6])

9. **Finish**

    * Summary + next steps; handles headless cases by printing port-forward instructions ([OpenClaw][6])

### What the wizard writes

The reference spells out typical writes:

* Config file: `~/.openclaw/openclaw.json` (agents defaults, gateway settings, channel tokens, skills install settings, wizard metadata)
* WhatsApp creds: `~/.openclaw/credentials/whatsapp/<accountId>/`
* Sessions: `~/.openclaw/agents/<agentId>/sessions/`
* It also notes some channels are plugins; wizard can prompt to install them before config ([OpenClaw][6])

### Non-interactive mode + RPC

Two “architecturally interesting” bits:

* **Non-interactive mode**: `openclaw onboard --non-interactive ...` for scripting, with optional JSON output. ([OpenClaw][6])
* **Wizard over RPC**: the Gateway exposes onboarding as RPC methods (`wizard.start`, `wizard.next`, `wizard.cancel`, `wizard.status`) so UIs (macOS app / Control UI) can reuse the same wizard logic instead of reimplementing it. ([OpenClaw][6])

That’s a strong design pattern: treat onboarding as a **shared protocol**, not “a CLI flow” that every UI must duplicate.

---

## A few takeaways to steal for your own generic agents

* **Separate routing from memory.** Decide *which brain* first, then *which conversation thread*.
* **Make IDs explicit and typed.** OpenClaw’s `{ kind: direct|group|channel, id }` prevents “stringly typed chaos” when routing. ([OpenClaw][1])
* **Use deterministic precedence rules.** “Most specific wins” makes debugging sane. ([OpenClaw][2])
* **Make background work a first-class session type.** Sub-agents are just sessions with different queueing + tool policy + announcement behavior. ([OpenClaw][4])
* **Treat onboarding as a reusable state machine.** Expose it via RPC so CLI + GUI stay consistent. ([OpenClaw][6])

If you’re aiming to build “generic agents” as a platform, OpenClaw’s trick is not mystical prompts—it’s **boring, explicit plumbing**: identifiers, routing tables, session keys, queues, and carefully gated capabilities. That’s the good stuff.

[1]: https://docs.openclaw.ai/gateway/configuration "Configuration - OpenClaw"
[2]: https://docs.openclaw.ai/concepts/multi-agent "Multi-Agent Routing - OpenClaw"
[3]: https://docs.openclaw.ai/concepts/session "Session Management - OpenClaw"
[4]: https://docs.openclaw.ai/tools/subagents "Sub-Agents - OpenClaw"
[5]: https://docs.openclaw.ai/concepts/session-tool "Session Tools - OpenClaw"
[6]: https://docs.openclaw.ai/reference/wizard "Onboarding Wizard Reference - OpenClaw"
