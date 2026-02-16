---
name: visualize-chat-ui
description: Capture a text snapshot of the Neuromancer chat TUI in demo mode via tmux
user_invocable: true
---

# Visualize Chat UI

Captures a text rendering of the `neuroctl orchestrator chat --demo` TUI using tmux, so you can see the chat interface without a TTY.

## Usage

Run the capture script:

```bash
.claude/skills/visualize-chat-ui/scripts/capture.sh
```

With navigation keystrokes (sent before capture):

```bash
.claude/skills/visualize-chat-ui/scripts/capture.sh --keys "Tab Up Up Space"
```

Custom dimensions and wait time:

```bash
.claude/skills/visualize-chat-ui/scripts/capture.sh --width 160 --height 50 --wait 3
```

## Interpreting the Output

The captured text is a plain-text rendering of the ratatui TUI. Layout regions:

- **Left sidebar**: Thread list (System0, sub-agent threads). The selected thread has a `>` marker or highlight characters.
- **Main pane** (center): Timeline of messages and tool invocations for the active thread.
- **Input pane** (bottom): Composer area with prompt. Shows "DEMO MODE" in the status bar.

### Badge Types in Timeline

Tool and message items show category badges in brackets:

| Badge | Meaning |
|-------|---------|
| `[TOOL]` | Runtime tool (list_agents, read_config) |
| `[ADAPT]` | Adaptive tool (propose_config_change, score_skills, analyze_failures, etc.) |
| `[AUTH-ADAPT]` | Authenticated adaptive tool (authorize_proposal, apply_authorized_proposal, modify_skill) |
| `[DELEGATE]` | delegate_to_agent invocation |
| `[SKILL]` | Skill-related tool |
| `[SYSTEM]` | System message (thread created, run state changed) |
| `[USER]` | User message |
| `[System0]` / `[Assistant]` | Assistant/System0 response |

Error items show with an error indicator. Collapsed items show a one-line summary; expanded items (toggled with Space) show full arguments/output JSON.

## TUI Keybindings Reference

Use `--keys` to send these as tmux keystrokes before capture:

| Key | tmux send-keys value | Action |
|-----|---------------------|--------|
| Tab | `Tab` | Cycle focus: Sidebar -> Main -> Input |
| Shift+Tab | `BTab` | Cycle focus backward |
| Up/Down | `Up` / `Down` | Navigate items in focused pane |
| Left/Right | `Left` / `Right` | Horizontal scroll in Main pane |
| Space | `Space` | Toggle expand/collapse selected item |
| Enter | `Enter` | Open selected thread (Sidebar) / follow link (Main) |
| 1-4 | `1` `2` `3` `4` | Filter: All / Delegates / Failures / System tools |
| q | `q` | Quit |

### Common Navigation Sequences

```bash
# Switch to sidebar and select second thread
--keys "BTab Up Enter"

# Scroll down in main pane and expand an item
--keys "Down Down Down Space"

# Filter to delegates only
--keys "2"

# Filter to failures only
--keys "3"
```
