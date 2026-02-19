#!/usr/bin/env bash
set -euo pipefail

# Capture a screenshot-style image of the neuroctl chat TUI in demo mode via tmux.
# Usage:
#   capture-screenshot.sh [--keys "Tab Up Space"] [--width 140] [--height 45] [--wait 2]
#                        [--out /tmp/chat.png]

SESSION="neuromancer-demo"
WIDTH=140
HEIGHT=45
WAIT=2
KEYS=""
OUT=""

usage() {
    cat <<'EOF'
Usage: capture-screenshot.sh [options]

Options:
  --keys "..."      Space-delimited tmux keys to send before capture (example: "Tab Down Space")
  --width N         tmux pane width (default: 140)
  --height N        tmux pane height (default: 45)
  --wait N          Seconds to wait for render (default: 2)
  --out PATH        Output PNG file path
  --help            Show this help

Examples:
  .claude/skills/visualize-chat-ui/scripts/capture-screenshot.sh
  .claude/skills/visualize-chat-ui/scripts/capture-screenshot.sh --keys "Down Down Space" --out /tmp/chat.png
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keys) KEYS="$2"; shift 2 ;;
        --width) WIDTH="$2"; shift 2 ;;
        --height) HEIGHT="$2"; shift 2 ;;
        --wait) WAIT="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

if ! command -v tmux >/dev/null; then
    echo "tmux is required." >&2
    exit 1
fi

if ! command -v uv >/dev/null; then
    echo "uv is required." >&2
    exit 1
fi

if [[ -z "$OUT" ]]; then
    OUT="/tmp/neuromancer-chat-$(date +%Y%m%d-%H%M%S).png"
fi

if [[ "$OUT" != *.png ]]; then
    echo "Only PNG output is supported. Use a .png file path with --out." >&2
    exit 1
fi
mkdir -p "$(dirname "$OUT")"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
BINARY="$REPO_ROOT/target/debug/neuroctl"
RENDER_SCRIPT="$SCRIPT_DIR/render_ansi_to_png.py"

if [[ ! -f "$RENDER_SCRIPT" ]]; then
    echo "Missing renderer script: $RENDER_SCRIPT" >&2
    exit 1
fi

if [[ ! -x "$BINARY" ]]; then
    echo "Building neuromancer-cli..." >&2
    (cd "$REPO_ROOT" && cargo build -p neuromancer-cli 2>&1) >&2
fi

ANSI_FILE="$(mktemp -t neuromancer-chat-ansi.XXXXXX)"

cleanup() {
    tmux send-keys -t "$SESSION" q 2>/dev/null || true
    sleep 0.2
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    rm -f "$ANSI_FILE"
}
trap cleanup EXIT

# Kill any stale session before starting a new one.
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x "$WIDTH" -y "$HEIGHT"
tmux send-keys -t "$SESSION" "$BINARY orchestrator chat --demo" Enter

sleep "$WAIT"

if [[ -n "$KEYS" ]]; then
    for key in $KEYS; do
        tmux send-keys -t "$SESSION" "$key"
        sleep 0.15
    done
    sleep 0.5
fi

# Capture the visible pane with ANSI styles and trailing spaces.
tmux capture-pane -t "$SESSION" -p -e -N > "$ANSI_FILE"

uv run "$RENDER_SCRIPT" "$ANSI_FILE" "$OUT" "$WIDTH" "$HEIGHT"

echo "Wrote screenshot: $OUT"
