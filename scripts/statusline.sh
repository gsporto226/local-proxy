#!/usr/bin/env bash
# Claude Code status line script. Reads the status-line JSON from stdin,
# extracts the session id, and asks local-proxy to render the status line for
# that session. Configure via settings.json:
#
#   "statusLine": { "type": "command", "command": "<abs path>/scripts/statusline.sh" }
#
# The template comes from the proxy config `statusline:` block or is passed
# with --template (flag overrides config).

set -euo pipefail

TPL=""            # optional: --template "..."
while [ "$#" -gt 0 ]; do
  case "$1" in
    --template) TPL="$2"; shift 2;;
    *) shift;;
  esac
done

# Read the status line JSON from stdin.
INPUT="$(cat 2>/dev/null || true)"

SESSION="$(printf '%s' "$INPUT" | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
MODEL="$(printf '%s' "$INPUT" | sed -n 's/.*"\(display_name\|model\)"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\2/p' | head -n1)"
CTX="$(printf '%s' "$INPUT" | sed -n 's/.*"used_percentage"[[:space:]]*:[[:space:]]*\([0-9.]*\).*/\1/p' | head -n1)"

ARGS=()
[ -n "$SESSION" ] && ARGS+=(--session "$SESSION")
[ -n "$MODEL" ]   && ARGS+=(--model "$MODEL")
[ -n "$CTX" ]     && ARGS+=(--context-pct "$CTX")
[ -n "$TPL" ]     && ARGS+=(--template "$TPL")

exec local-proxy statusline "${ARGS[@]}"
