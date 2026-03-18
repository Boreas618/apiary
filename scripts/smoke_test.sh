#!/usr/bin/env bash
#
# Apiary smoke test — quickly verifies the daemon is healthy and sandboxes work.
# Usage: ./scripts/smoke_test.sh [http://host:port]
#
set -uo pipefail

URL="${1:-http://127.0.0.1:8080}"
PASS=0
FAIL=0
TOTAL=0

red()   { printf '\033[1;31m%s\033[0m\n' "$*"; }
green() { printf '\033[1;32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

assert_json() {
    local label="$1" json="$2" expr="$3"
    TOTAL=$((TOTAL + 1))
    if [ -z "$json" ]; then
        red "  FAIL  $label (empty response)"
        FAIL=$((FAIL + 1))
        return
    fi
    if echo "$json" | python3 -c "
import sys, json
d = json.load(sys.stdin)
$expr
" 2>/dev/null; then
        green "  PASS  $label"
        PASS=$((PASS + 1))
    else
        red "  FAIL  $label"
        FAIL=$((FAIL + 1))
    fi
}

api_get() { curl -sf "${URL}$1" 2>/dev/null || echo ''; }

api_post() { curl -sf -X POST -H 'Content-Type: application/json' -d "$2" "${URL}$1" 2>/dev/null || echo ''; }

api_delete() { curl -sf -o /dev/null -w '%{http_code}' -X DELETE "${URL}$1" 2>/dev/null || echo '0'; }

# Send a command to a session. Uses jq if available for safe JSON encoding,
# otherwise falls back to simple string embedding.
exec_cmd() {
    local sid="$1" cmd="$2" tms="${3:-30000}"
    local payload
    if command -v jq >/dev/null 2>&1; then
        payload=$(jq -n --arg c "$cmd" --arg s "$sid" --argjson t "$tms" \
            '{command: $c, session_id: $s, timeout_ms: $t}')
    else
        local escaped_cmd
        escaped_cmd=$(printf '%s' "$cmd" | sed 's/\\/\\\\/g; s/"/\\"/g')
        payload="{\"command\":\"$escaped_cmd\",\"session_id\":\"$sid\",\"timeout_ms\":$tms}"
    fi
    api_post /api/v1/tasks "$payload"
}

# Wrap a shell command so redirects, pipes, $VAR etc. work via the REST API.
sh_cmd() {
    local escaped
    escaped=$(printf '%s' "$1" | sed "s/'/'\\\\''/g")
    printf "/bin/sh -c '%s'" "$escaped"
}

bold "========================================="
bold "  Apiary Smoke Test"
bold "========================================="
echo "  Target: $URL"
echo ""

# ── 1. Health ──
TOTAL=$((TOTAL + 1))
if api_get /healthz | grep -q ok; then
    green "  PASS  healthz endpoint"
    PASS=$((PASS + 1))
else
    red "  FAIL  healthz endpoint"; FAIL=$((FAIL + 1))
fi

# ── 2. Pool status ──
bold "--- Pool Status ---"
STATUS=$(api_get /api/v1/status || echo '{}')
echo "  $STATUS"

assert_json "status returns valid JSON" "$STATUS" "pass"
assert_json "has idle sandboxes"        "$STATUS" "assert d.get('idle',0) > 0, d"

# ── 3. Create session ──
bold "--- Session ---"
S_RESP=$(api_post /api/v1/sessions '{}' || echo '{}')
SID=$(echo "$S_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin).get('session_id',''))" 2>/dev/null || true)

TOTAL=$((TOTAL + 1))
if [ -n "$SID" ]; then
    green "  PASS  create session"
    PASS=$((PASS + 1))
    echo "  session_id: $SID"
else
    red "  FAIL  create session ($S_RESP)"
    FAIL=$((FAIL + 1))
    exit 1
fi

# ── 4. Basic execution ──
bold "--- Command Execution ---"
R=$(exec_cmd "$SID" "echo hello-apiary")
assert_json "echo succeeds"         "$R" "assert d['success'], d"
assert_json "stdout captured"       "$R" "assert 'hello-apiary' in d['stdout'], d['stdout']"

# ── 5. Shell features (redirects, pipes, variables) ──
bold "--- Shell Features ---"
R=$(exec_cmd "$SID" "$(sh_cmd 'echo test-data > /workspace/.smoke-test && cat /workspace/.smoke-test')")
assert_json "redirect + read back"  "$R" "assert 'test-data' in d['stdout'], d"

R=$(exec_cmd "$SID" "$(sh_cmd 'echo hello | tr h H')")
assert_json "pipe works"            "$R" "assert 'Hello' in d['stdout'], d"

R=$(exec_cmd "$SID" "$(sh_cmd 'echo $PATH')")
assert_json "variable expansion"    "$R" "assert '/usr/bin' in d['stdout'], d['stdout']"

R=$(exec_cmd "$SID" "$(sh_cmd 'echo err-msg >&2')")
assert_json "stderr captured"       "$R" "assert 'err-msg' in d['stderr'], d['stderr']"

# ── 6. Filesystem ──
bold "--- Filesystem ---"
R=$(exec_cmd "$SID" "$(sh_cmd 'cat /workspace/.smoke-test')")
assert_json "file persists in session" "$R" "assert 'test-data' in d['stdout'], d"

R=$(exec_cmd "$SID" "ls /workspace")
assert_json "/workspace exists"        "$R" "assert d['success'], d"

R=$(exec_cmd "$SID" "pwd")
assert_json "workdir is /workspace"    "$R" "assert '/workspace' in d['stdout'], d['stdout']"

# ── 7. Process isolation ──
bold "--- Isolation ---"
R=$(exec_cmd "$SID" "id")
assert_json "id command works"      "$R" "assert d['success'], d"

R=$(exec_cmd "$SID" "$(sh_cmd 'cat /proc/1/cmdline 2>/dev/null || echo no-access')")
assert_json "host PID 1 hidden"     "$R" "assert 'apiary' not in (d['stdout']+d['stderr']).lower(), d"

# ── 8. Exit codes ──
bold "--- Exit Codes ---"
R=$(exec_cmd "$SID" "true")
assert_json "exit 0 for true"      "$R" "assert d['exit_code'] == 0, d['exit_code']"

R=$(exec_cmd "$SID" "false")
assert_json "exit 1 for false"     "$R" "assert d['exit_code'] != 0, d['exit_code']"

# ── 9. Timeout ──
bold "--- Timeout ---"
R=$(exec_cmd "$SID" "sleep 60" 2000)
assert_json "task times out"        "$R" "assert d['timed_out'], d"

# ── 10. Cleanup ──
bold "--- Cleanup ---"
TOTAL=$((TOTAL + 1))
CODE=$(api_delete "/api/v1/sessions/$SID")
if [ "$CODE" = "204" ]; then
    green "  PASS  close session"
    PASS=$((PASS + 1))
else
    red "  FAIL  close session (http $CODE)"
    FAIL=$((FAIL + 1))
fi

# New session — verify sandbox was reset
S2_RESP=$(api_post /api/v1/sessions '{}' || echo '{}')
SID2=$(echo "$S2_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin).get('session_id',''))" 2>/dev/null || true)
if [ -n "$SID2" ]; then
    R=$(exec_cmd "$SID2" "$(sh_cmd 'cat /workspace/.smoke-test 2>&1; echo done')")
    assert_json "sandbox reset (old files gone)" "$R" "assert 'test-data' not in d['stdout'], 'file survived reset'"
    api_delete "/api/v1/sessions/$SID2" >/dev/null 2>&1 || true
fi

# ── Summary ──
echo ""
bold "========================================="
if [ "$FAIL" -eq 0 ]; then
    green "  ALL $TOTAL TESTS PASSED"
else
    red "  $FAIL/$TOTAL TESTS FAILED"
fi
bold "========================================="
exit "$FAIL"
