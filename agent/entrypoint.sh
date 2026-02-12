#!/bin/bash
set -euo pipefail

CLAUDE_HOME="${HOME:-/root}"
if [ -f /mnt/claude-auth/credentials.json ]; then
    mkdir -p "$CLAUDE_HOME/.claude"
    cp /mnt/claude-auth/credentials.json "$CLAUDE_HOME/.claude/.credentials.json"
    chmod 600 "$CLAUDE_HOME/.claude/.credentials.json"
fi

if [ -f /mnt/claude-auth/claude.json ]; then
    cp /mnt/claude-auth/claude.json "$CLAUDE_HOME/.claude.json"
else
    echo '{"hasCompletedOnboarding":true}' > "$CLAUDE_HOME/.claude.json"
fi

mkdir -p "$CLAUDE_HOME/.claude"
touch "$CLAUDE_HOME/.claude/remote-settings.json"

PROMPT_PATH="${YUI_PROMPT_PATH:-/workspace/prompt.txt}"
if [ ! -f "$PROMPT_PATH" ]; then
    echo '{"type":"error","message":"prompt file not found","retryable":false}'
    exit 1
fi

PROMPT=$(cat "$PROMPT_PATH")
if [ -z "$PROMPT" ]; then
    echo '{"type":"error","message":"empty prompt","retryable":false}'
    exit 1
fi

MAX_TURNS="${YUI_MAX_TURNS:-10}"

SESSION_ID="${YUI_SESSION_ID:-$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || echo "no-session")}"
echo "{\"type\":\"session\",\"session_id\":\"${SESSION_ID}\"}"

# snapshot workspace before claude runs
find /workspace -type f | sort > /tmp/before_files.txt 2>/dev/null || true

RESULT=$(claude --print \
    --output-format json \
    --dangerously-skip-permissions \
    --no-session-persistence \
    --max-turns "$MAX_TURNS" \
    -p "$PROMPT" 2>/tmp/claude-stderr) || true

STDERR_CONTENT=$(cat /tmp/claude-stderr 2>/dev/null || true)
if [ -n "$STDERR_CONTENT" ]; then
    while IFS= read -r line; do
        ESCAPED=$(echo "$line" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))' 2>/dev/null || echo "\"$line\"")
        echo "{\"type\":\"log\",\"stream\":\"stderr\",\"line\":${ESCAPED}}"
    done <<< "$STDERR_CONTENT"
fi

if [ -z "$RESULT" ]; then
    echo '{"type":"error","message":"claude returned empty result","retryable":true}'
    exit 1
fi

# find new files created by claude
find /workspace -type f | sort > /tmp/after_files.txt 2>/dev/null || true
NEW_FILES=$(comm -13 /tmp/before_files.txt /tmp/after_files.txt 2>/dev/null || true)

# build the final frame with attachments for new files
OUTPUT=$(echo "$RESULT" | python3 -c "
import json, sys, os, mimetypes

new_files_raw = '''${NEW_FILES}'''
new_files = [f.strip() for f in new_files_raw.strip().split('\n') if f.strip()]

attachments = []
for fpath in new_files:
    if not os.path.isfile(fpath):
        continue
    name = os.path.basename(fpath)
    mime, _ = mimetypes.guess_type(fpath)
    if not mime:
        mime = 'application/octet-stream'
    size = os.path.getsize(fpath)
    # skip tiny files and prompt.txt
    if name == 'prompt.txt' or size < 10:
        continue
    ftype = mime.split('/')[0] if mime.split('/')[0] in ('image','video','audio') else 'document'
    attachments.append({
        'type': ftype,
        'path': fpath,
        'name': name,
        'mime': mime,
        'size': size
    })

try:
    r = json.load(sys.stdin)
    if r.get('is_error'):
        msg = r.get('result', 'unknown error')
        print(json.dumps({'type': 'error', 'message': msg, 'retryable': False}))
    else:
        text = r.get('result', '')
        if text:
            print(json.dumps({'type': 'final', 'output': text, 'attachments': attachments}))
        else:
            print(json.dumps({'type': 'error', 'message': 'no output from claude', 'retryable': False}))
except Exception as e:
    print(json.dumps({'type': 'error', 'message': str(e), 'retryable': False}))
" 2>/dev/null)

if [ -z "$OUTPUT" ]; then
    echo '{"type":"error","message":"failed to parse claude output","retryable":false}'
    exit 1
fi

echo "$OUTPUT"
