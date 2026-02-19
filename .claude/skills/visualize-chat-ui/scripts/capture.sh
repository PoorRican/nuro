#!/usr/bin/env bash
set -euo pipefail

# Capture a text snapshot of the neuroctl chat TUI in demo mode via tmux.
# Usage: capture.sh [--keys "Tab Up Space"] [--width 140] [--height 45] [--wait 2]

SESSION="neuromancer-demo"
WIDTH=140
HEIGHT=45
WAIT=2
KEYS=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keys)  KEYS="$2"; shift 2 ;;
        --width) WIDTH="$2"; shift 2 ;;
        --height) HEIGHT="$2"; shift 2 ;;
        --wait)  WAIT="$2"; shift 2 ;;
        *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

# Kill any stale session
tmux kill-session -t "$SESSION" 2>/dev/null || true

# Build the CLI binary if needed
BINARY="$(cd "$(dirname "$0")/../../../.." && pwd)/target/debug/neuroctl"
if [[ ! -x "$BINARY" ]]; then
    echo "Building neuromancer-cli..." >&2
    (cd "$(dirname "$0")/../../../.." && cargo build -p neuromancer-cli 2>&1) >&2
fi

# Create detached tmux session with specified dimensions
tmux new-session -d -s "$SESSION" -x "$WIDTH" -y "$HEIGHT"

# Launch demo chat
tmux send-keys -t "$SESSION" "$BINARY orchestrator chat --demo" Enter

# Wait for TUI to render
sleep "$WAIT"

# Send optional navigation keystrokes
if [[ -n "$KEYS" ]]; then
    for key in $KEYS; do
        tmux send-keys -t "$SESSION" "$key"
        sleep 0.15
    done
    sleep 0.5
fi

# Capture the pane content
tmux capture-pane -t "$SESSION" -p

# Quit the TUI and kill the session
tmux send-keys -t "$SESSION" q
sleep 0.3
tmux kill-session -t "$SESSION" 2>/dev/null || true
