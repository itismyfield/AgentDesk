#!/usr/bin/env bash
# Mutation proof for #5274 slice A: notification, confirmed-mode issue creation,
# and E-1 operator overrides must all return through a fixed bound.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
RELAY_PY="$REPO_ROOT/scripts/e2e/run_tui_relay.py"
MATRIX_PY="$REPO_ROOT/scripts/e2e/run_multi_provider_matrix.py"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-bounded-calls-5274.XXXXXX")
LISTENER_PIDS=()
FAILURES=0
SKIPPED_CASES=0
ISSUE_CAPTURE_LIMIT=4096
ISSUE_CAPTURE_READ_LIMIT=$((ISSUE_CAPTURE_LIMIT + 1))

cleanup() {
    local listener_pid
    # Each listener has a self-expiring deadline; waiting reaps only fixtures
    # created by this test and never sends a signal to any process.
    for listener_pid in "${LISTENER_PIDS[@]-}"; do
        [ -n "$listener_pid" ] || continue
        wait "$listener_pid" 2>/dev/null || true
    done
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

extract_function() {
    local source_path="$1"
    local function_name="$2"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$source_path"
}

record_skipped_cases() {
    local count="$1"
    SKIPPED_CASES=$((SKIPPED_CASES + count))
}

record_skipped_cases 1
if [ "$SKIPPED_CASES" -ne 1 ]; then
    echo "FAIL: skipped-case accounting did not preserve a non-PASS result" >&2
    FAILURES=$((FAILURES + 1))
fi
SKIPPED_CASES=0

if ! grep -Fq 'curl -sf --connect-timeout 2 --max-time 15 -X POST' "$DEPLOY_SH"; then
    echo "FAIL: _notify_channel is missing the fixed connect/total timeout" >&2
    FAILURES=$((FAILURES + 1))
fi
if ! grep -Fq 'python3 "$SCRIPT_DIR/ci-timeout.py" 10 gh issue create' "$DEPLOY_SH"; then
    echo "FAIL: confirmed-mode gh issue create is missing the repository timeout runner" >&2
    FAILURES=$((FAILURES + 1))
fi
if ! grep -Fq '# Byte-count guarantees cover NUL-free text only; command substitution drops NULs and can undercount.' "$DEPLOY_SH"; then
    echo "FAIL: NUL-output byte-count non-guarantee is missing or changed" >&2
    FAILURES=$((FAILURES + 1))
fi

start_hanging_listener() {
    local ready_path="$1"
    # The listener accepts TCP, consumes no response, and exits on its own after
    # 18s. The production 15s cap therefore has a margin for macOS diagnostics;
    # a removed cap remains alive when the test's 17s assertion deadline fires.
    python3 - "$ready_path" 2>/dev/null <<'PY' &
import socket
import sys
import time
from pathlib import Path

ready_path = Path(sys.argv[1])
deadline = time.monotonic() + 18.0
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as server:
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    ready_path.write_text(str(server.getsockname()[1]), encoding="ascii")
    server.settimeout(0.1)
    connection = None
    while connection is None and time.monotonic() < deadline:
        try:
            connection, _ = server.accept()
        except socket.timeout:
            pass
    if connection is not None:
        with connection:
            connection.settimeout(0.1)
            while time.monotonic() < deadline:
                try:
                    chunk = connection.recv(65536)
                    if not chunk:
                        time.sleep(0.05)
                except socket.timeout:
                    pass
                except OSError:
                    break
PY
    LISTENER_PIDS+=("$!")
    for _ in {1..200}; do
        [ -s "$ready_path" ] && return 0
        sleep 0.01
    done
    echo "NOTE: hanging listener did not publish an ephemeral port" >&2
    return 1
}

measure_case() {
    local case_path="$1"
    local label="$2"
    local output_path="$TMP_ROOT/${label}.measure.log"
    local rc=0
    # The Python supervisor observes completion without terminating the child;
    # the fixture listener's own deadline provides eventual cleanup for a mutant.
    python3 - "$case_path" "$output_path" <<'PY' >"$output_path" 2>&1 || rc=$?
import subprocess
import sys
import time

case_path, output_path = sys.argv[1:]
assertion_deadline_s = 17.0
started = time.monotonic()
process = subprocess.Popen(
    ["bash", case_path],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    text=True,
)
deadline = started + assertion_deadline_s
while process.poll() is None and time.monotonic() < deadline:
    time.sleep(0.02)
timed_out = process.poll() is None
try:
    return_code = process.wait(timeout=8)
except subprocess.TimeoutExpired:
    # No signal-based cleanup is permitted here. The test fixture is designed
    # to close the accepted socket before this wait expires.
    return_code = 125
elapsed = time.monotonic() - started
if process.stdout is not None:
    output = process.stdout.read()
else:
    output = ""
sys.stdout.write(output)
sys.stdout.write(
    f"measure label={case_path} elapsed={elapsed:.3f}s rc={return_code} "
    f"timeout_assertion={'FAILED' if timed_out else 'ok'}\n"
)
if timed_out or return_code != 0:
    raise SystemExit(1)
PY
    cat "$output_path"
    return "$rc"
}

write_notify_case() {
    local source_path="$1"
    local port="$2"
    local case_path="$3"
    {
        printf '%s\n' 'set -euo pipefail'
        extract_function "$source_path" _notify_channel
        printf '%s\n' \
            'REPORT_CHANNEL_ID=bounded-test' \
            'ADK_DEFAULT_LOOPBACK=127.0.0.1' \
            "REL_PORT=$port" \
            '_notify_channel "bounded notification fixture"'
    } > "$case_path"
    chmod +x "$case_path"
}

write_issue_case() {
    local source_path="$1"
    local port="$2"
    local case_path="$3"
    mkdir -p "$TMP_ROOT/release/logs" "$TMP_ROOT/bin"
    cat > "$TMP_ROOT/bin/gh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
curl -sS -X POST "http://127.0.0.1:${port}/issue" --data-binary 'issue fixture' >/dev/null
printf '%s\\n' 'https://example.invalid/issues/5274'
EOF
    chmod +x "$TMP_ROOT/bin/gh"
    {
        printf '%s\n' 'set -euo pipefail'
        extract_function "$source_path" _notify_channel
        extract_function "$source_path" _report_post_deploy_smoke_failure
        printf '%s\n' \
            "PATH=$TMP_ROOT/bin:\$PATH" \
            "SCRIPT_DIR=$REPO_ROOT/scripts" \
            "REPO=$REPO_ROOT" \
            "ADK_REL=$TMP_ROOT/release" \
            "REL_PORT=$port" \
            'REPORT_CHANNEL_ID=' \
            'POST_DEPLOY_SMOKE_CREATE_ISSUE=confirmed' \
            'POST_DEPLOY_SMOKE_STAMP=bounded-5274' \
            "POST_DEPLOY_SMOKE_EVIDENCE=$TMP_ROOT/evidence.log" \
            'POST_DEPLOY_SMOKE_FAILURES=("fixture listener did not answer")' \
            '_report_post_deploy_smoke_failure'
    } > "$case_path"
    chmod +x "$case_path"
}

write_issue_output_case() {
    local source_path="$1"
    local output_mode="$2"
    local label="$3"
    local case_path="$TMP_ROOT/${label}.sh"
    local bin="$TMP_ROOT/${label}-bin"
    mkdir -p "$TMP_ROOT/release/logs" "$bin"
    cat > "$bin/gh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
case "$output_mode" in
    truncated)
        python3 -c 'import sys; p="https://github.com/itismyfield/AgentDesk/issues/"; sys.stdout.write(p + "7" * (8192 - len(p)))'
        ;;
    exact)
        python3 -c 'import sys; p="https://github.com/itismyfield/AgentDesk/issues/"; sys.stdout.write(p + "7" * (4096 - len(p)))'
        ;;
    nul)
        python3 -c 'import sys; from pathlib import Path; out=b"https://github.com/itismyfield/AgentDesk/issues/5274\n"+b"\0"*5000; err=b"\0"*5000; Path(sys.argv[1]).write_text(f"{len(out)} {len(err)}\n"); sys.stdout.buffer.write(out); sys.stderr.buffer.write(err)' "$TMP_ROOT/${label}-nul-counts"
        exit 0
        ;;
    empty) : ;;
    failure)
        printf '%s\\n' 'https://github.com/itismyfield/AgentDesk/issues/5274'
        exit 1
        ;;
    partial) printf '%s' 'https://github.com/itismyfield/AgentDesk/issu' ;;
    *) printf '%s\\n' 'https://github.com/itismyfield/AgentDesk/issues/5274' ;;
esac
printf '%s\\n' 'https://stderr.example.invalid/first' >&2
EOF
    chmod +x "$bin/gh"
    {
        printf '%s\n' 'set -euo pipefail'
        extract_function "$source_path" _notify_channel
        extract_function "$source_path" _report_post_deploy_smoke_failure
        printf '%s\n' \
            "PATH=$bin:\$PATH" \
            "SCRIPT_DIR=$REPO_ROOT/scripts" \
            "REPO=$REPO_ROOT" \
            "ADK_REL=$TMP_ROOT/release" \
            'REL_PORT=0' \
            'REPORT_CHANNEL_ID=' \
            'POST_DEPLOY_SMOKE_CREATE_ISSUE=confirmed' \
            "POST_DEPLOY_SMOKE_STAMP=bounded-5274-$label" \
            "POST_DEPLOY_SMOKE_EVIDENCE=$TMP_ROOT/$label-evidence.log" \
            'POST_DEPLOY_SMOKE_FAILURES=("fixture output")' \
            '_report_post_deploy_smoke_failure'
    } > "$case_path"
    chmod +x "$case_path"
}

write_issue_stderr_case() {
    local source_path="$1"
    local label="$2"
    local stderr_bytes="$3"
    local case_path="$TMP_ROOT/${label}.sh"
    local bin="$TMP_ROOT/${label}-bin"
    mkdir -p "$TMP_ROOT/release/logs" "$bin"
    cat > "$bin/gh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
python3 -c 'import sys; sys.stderr.write("e" * $stderr_bytes)'
exit 1
EOF
    chmod +x "$bin/gh"
    {
        printf '%s\n' 'set -euo pipefail'
        extract_function "$source_path" _notify_channel
        extract_function "$source_path" _report_post_deploy_smoke_failure
        printf '%s\n' \
            "PATH=$bin:\$PATH" \
            "SCRIPT_DIR=$REPO_ROOT/scripts" \
            "REPO=$REPO_ROOT" \
            "ADK_REL=$TMP_ROOT/release" \
            'REL_PORT=0' \
            'REPORT_CHANNEL_ID=' \
            'POST_DEPLOY_SMOKE_CREATE_ISSUE=confirmed' \
            "POST_DEPLOY_SMOKE_STAMP=bounded-5274-$label" \
            "POST_DEPLOY_SMOKE_EVIDENCE=$TMP_ROOT/$label-evidence.log" \
            'POST_DEPLOY_SMOKE_FAILURES=("fixture stderr")' \
            '_report_post_deploy_smoke_failure'
    } > "$case_path"
    chmod +x "$case_path"
}

run_issue_output_variant() {
    local label="$1"
    local source_path="$2"
    local output_mode="$3"
    local expected="$4"
    local case_path="$TMP_ROOT/${label}.sh"
    local output_path="$TMP_ROOT/${label}.out"
    local evidence_path="$TMP_ROOT/${label}-evidence.log"
    local nul_counts_path="$TMP_ROOT/${label}-nul-counts"
    local rc=0
    write_issue_output_case "$source_path" "$output_mode" "$label" "$case_path"
    bash "$case_path" > "$output_path" 2>&1 || rc=$?
    cat "$output_path"
    if [ "$expected" = value ] && [ "$rc" -eq 0 ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://github.com/itismyfield/AgentDesk/issues/5274' "$output_path" \
        && ! grep -qF 'https://stderr.example.invalid/first' "$output_path"; then
        echo "issue_url value restored: ok (stdout URL survived stderr noise)"
    elif [ "$expected" = truncation ] && [ "$rc" -eq 0 ] \
        && grep -qF 'returned truncated stdout' "$output_path" \
        && ! grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$output_path"; then
        echo "stdout truncation detection restored: ok (no URL reported)"
    elif [ "$expected" = exact ] && [ "$rc" -eq 0 ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://github.com/itismyfield/AgentDesk/issues/' "$output_path" \
        && ! grep -qF 'returned truncated stdout' "$output_path"; then
        echo "exact-cap stdout restored: ok (4096 bytes not reported as truncated)"
    elif [ "$expected" = nul_unsupported ] && [ "$rc" -eq 0 ] \
        && [ "$(tr -d '\n' < "$nul_counts_path")" = '5053 5000' ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://github.com/itismyfield/AgentDesk/issues/5274' "$output_path" \
        && ! grep -qF 'returned truncated stdout' "$output_path" \
        && ! grep -aFq '[gh stderr truncated at 4096 bytes]' "$evidence_path" \
        && [ ! -s "$evidence_path" ]; then
        echo "NUL-output non-guarantee pinned: ok (5053-byte stdout accepted; 5000-byte stderr marker absent)"
    elif [ "$expected" = empty ] && [ "$rc" -eq 0 ] \
        && grep -qF 'returned empty stdout' "$output_path" \
        && ! grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$output_path"; then
        echo "empty stdout guard restored: ok"
    elif [ "$expected" = invalid ] && [ "$rc" -eq 0 ] \
        && grep -qF 'returned invalid stdout' "$output_path" \
        && ! grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$output_path"; then
        echo "partial stdout guard restored: ok (malformed URL rejected)"
    elif [ "$expected" = failure ] && [ "$rc" -eq 0 ] \
        && grep -qF 'issue creation FAILED' "$output_path" \
        && ! grep -qF 'returned empty stdout' "$output_path" \
        && ! grep -qF 'returned invalid stdout' "$output_path"; then
        echo "gh failure gate restored: ok"
    else
        echo "FAIL: issue output assertion failed for $label (rc=$rc)" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

run_issue_output_mutant() {
    local label="$1"
    local source_path="$2"
    local output_mode="$3"
    local mutant_kind="$4"
    local case_path="$TMP_ROOT/${label}.sh"
    local output_path="$TMP_ROOT/${label}.out"
    local rc=0
    write_issue_output_case "$source_path" "$output_mode" "$label" "$case_path"
    bash "$case_path" > "$output_path" 2>&1 || rc=$?
    cat "$output_path"
    if [ "$mutant_kind" = value ] \
        && ! grep -qF 'https://github.com/itismyfield/AgentDesk/issues/5274' "$output_path"; then
        echo "issue_url value mutant: FAILED (self-assertion: fixture URL was not reported)"
    elif [ "$mutant_kind" = truncation ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://github.com/itismyfield/AgentDesk/issues/' "$output_path"; then
        echo "stdout truncation mutant: FAILED (self-assertion: truncated URL was reported)"
    elif [ "$mutant_kind" = merge ] \
        && ! grep -qF 'Post-deploy smoke issue created (confirmed mode): https://github.com/itismyfield/AgentDesk/issues/5274' "$output_path"; then
        echo "stdout/stderr merge mutant: FAILED (self-assertion: expected stdout URL was not reported)"
    elif [ "$mutant_kind" = empty ] \
        && ! grep -qF 'returned empty stdout' "$output_path"; then
        echo "empty stdout guard mutant: FAILED (self-assertion: empty result was misclassified)"
    elif [ "$mutant_kind" = rc ] \
        && ! grep -qF 'issue creation FAILED' "$output_path"; then
        echo "gh failure gate mutant: FAILED (self-assertion: nonzero gh result was accepted)"
    else
        echo "FAIL: $mutant_kind mutant survived its value/truncation assertion (rc=$rc)" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

run_issue_stderr_variant() {
    local label="$1"
    local source_path="$2"
    local expected="$3"
    local stderr_bytes="$4"
    local case_path="$TMP_ROOT/${label}.sh"
    local output_path="$TMP_ROOT/${label}.out"
    local evidence_path="$TMP_ROOT/${label}-evidence.log"
    local marker='[gh stderr truncated at 4096 bytes]'
    local max_bytes=$((ISSUE_CAPTURE_LIMIT + ${#marker} + 2))
    local actual_bytes=0 rc=0
    write_issue_stderr_case "$source_path" "$label" "$stderr_bytes"
    bash "$case_path" > "$output_path" 2>&1 || rc=$?
    actual_bytes=$(stat -f%z "$evidence_path" 2>/dev/null || stat -c%s "$evidence_path")
    if [ "$expected" = ok ] && [ "$rc" -eq 0 ] \
        && grep -aFq "$marker" "$evidence_path" \
        && [ "$actual_bytes" -le "$max_bytes" ]; then
        echo "stderr evidence cap restored: ok (bytes=$actual_bytes <= $max_bytes)"
    elif [ "$expected" = exact ] && [ "$rc" -eq 0 ] \
        && ! grep -aFq "$marker" "$evidence_path" \
        && [ "$actual_bytes" -eq "$ISSUE_CAPTURE_LIMIT" ]; then
        echo "exact-cap stderr restored: ok (4096 bytes without false truncation marker)"
    elif [ "$expected" = failed ] && { [ "$actual_bytes" -gt "$max_bytes" ] || ! grep -aFq "$marker" "$evidence_path"; }; then
        echo "stderr evidence cap removed: FAILED (self-assertion: bytes=$actual_bytes)"
    else
        echo "FAIL: stderr evidence cap assertion failed for $label (rc=$rc bytes=$actual_bytes)" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

write_issue_writer_case() {
    local source_path="$1"
    local mode="$2"
    local linger_s="$3"
    local label="$4"
    local case_path="$5"
    local observation_path="$6"
    local install_capture_wrappers="$7"
    local bin="$TMP_ROOT/${label}-bin"
    local ready_path="$TMP_ROOT/${label}.ready"
    local trigger_path="$TMP_ROOT/${label}.trigger"
    local grown_path="$TMP_ROOT/${label}.grown"
    local real_head real_cat real_wc
    real_head="$(command -v head)"
    real_cat="$(command -v cat)"
    real_wc="$(command -v wc)"
    mkdir -p "$TMP_ROOT/release/logs" "$bin"
    cat > "$bin/gh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# The gh leader exits after its child has written the initial payload. The child
# therefore keeps gh's stdout descriptor open after the leader is gone.
(
    python3 - "$ready_path" "$mode" "$linger_s" "$trigger_path" "$grown_path" <<'PY'
import sys
import time
from pathlib import Path

ready_path = Path(sys.argv[1])
mode = sys.argv[2]
linger_s = float(sys.argv[3])
trigger_path = Path(sys.argv[4])
grown_path = Path(sys.argv[5])
prefix = b"https://github.com/itismyfield/AgentDesk/issues/"
payload = prefix + (b"7" if mode == "race" else b"5274\\n" + (b"f" * 8192))
sys.stdout.buffer.write(payload)
sys.stdout.buffer.flush()
ready_path.write_text("ready", encoding="ascii")
if mode == "race":
    deadline = time.monotonic() + linger_s
    while not trigger_path.exists() and time.monotonic() < deadline:
        time.sleep(0.005)
    if trigger_path.exists():
        sys.stdout.buffer.write(b"7" * 8192)
        sys.stdout.buffer.flush()
        grown_path.write_text("grown", encoding="ascii")
elif mode == "finite":
    time.sleep(linger_s)
else:
    deadline = time.monotonic() + linger_s
    while time.monotonic() < deadline:
        sys.stdout.buffer.write(b"p" * 1024)
        sys.stdout.buffer.flush()
        time.sleep(0.01)
PY
) &
for _ in {1..200}; do
    [ -f "$ready_path" ] && break
    sleep 0.01
done
[ -f "$ready_path" ]
exit 0
EOF
    chmod +x "$bin/gh"
    if [ "$install_capture_wrappers" -eq 1 ]; then
        cat > "$bin/head" <<EOF
#!/usr/bin/env bash
set -euo pipefail
target="\${@: -1}"
if [[ "\$target" == *agentdesk-issue-stdout* ]]; then
    if [ "$mode" = race ]; then
        : > "$trigger_path"
        for _ in {1..400}; do
            [ -f "$grown_path" ] && break
            sleep 0.005
        done
        [ -f "$grown_path" ]
    fi
    "$real_head" "\$@" > "$TMP_ROOT/${label}.capture"
    "$real_wc" -c < "$TMP_ROOT/${label}.capture" > "$observation_path"
    "$real_cat" "$TMP_ROOT/${label}.capture"
else
    exec "$real_head" "\$@"
fi
EOF
        chmod +x "$bin/head"
        cat > "$bin/cat" <<EOF
#!/usr/bin/env bash
set -euo pipefail
target="\${@: -1}"
if [[ "\$target" == *agentdesk-issue-stdout* ]]; then
    if [ "$mode" = race ]; then
        : > "$trigger_path"
        for _ in {1..400}; do
            [ -f "$grown_path" ] && break
            sleep 0.005
        done
        [ -f "$grown_path" ]
    fi
    "$real_cat" "\$@" > "$TMP_ROOT/${label}.capture"
    "$real_wc" -c < "$TMP_ROOT/${label}.capture" > "$observation_path"
    "$real_cat" "$TMP_ROOT/${label}.capture"
else
    exec "$real_cat" "\$@"
fi
EOF
        chmod +x "$bin/cat"
    fi
    {
        printf '%s\n' 'set -euo pipefail'
        extract_function "$source_path" _notify_channel
        extract_function "$source_path" _report_post_deploy_smoke_failure
        printf '%s\n' \
            "PATH=$bin:\$PATH" \
            "SCRIPT_DIR=$REPO_ROOT/scripts" \
            "REPO=$REPO_ROOT" \
            "ADK_REL=$TMP_ROOT/release" \
            'REL_PORT=0' \
            'REPORT_CHANNEL_ID=' \
            'POST_DEPLOY_SMOKE_CREATE_ISSUE=confirmed' \
            "POST_DEPLOY_SMOKE_STAMP=bounded-5274-$label" \
            "POST_DEPLOY_SMOKE_EVIDENCE=$TMP_ROOT/$label-evidence.log" \
            "POST_DEPLOY_SMOKE_FAILURES=(\"$mode writer kept stdout open after leader exit\")" \
            '_report_post_deploy_smoke_failure'
    } > "$case_path"
    chmod +x "$case_path"
}

run_notify_variant() {
    local label="$1"
    local source_path="$2"
    local expected="$3"
    local ready_path="$TMP_ROOT/${label}.port"
    local case_path="$TMP_ROOT/${label}.sh"
    start_hanging_listener "$ready_path"
    local port
    port="$(<"$ready_path")"
    write_notify_case "$source_path" "$port" "$case_path"
    local rc=0
    measure_case "$case_path" "$label" || rc=$?
    wait "${LISTENER_PIDS[${#LISTENER_PIDS[@]}-1]}" 2>/dev/null || true
    if [ "$expected" = "ok" ]; then
        if [ "$rc" -ne 0 ]; then
            echo "FAIL: restored _notify_channel guard exceeded its 15s bound" >&2
            FAILURES=$((FAILURES + 1))
        else
            echo "_notify_channel restored: ok"
        fi
    elif [ "$rc" -eq 0 ]; then
        echo "FAIL: removing _notify_channel timeout did not fail the timeout assertion" >&2
        FAILURES=$((FAILURES + 1))
    else
        echo "_notify_channel removed: FAILED (timeout assertion)"
    fi
}

run_issue_variant() {
    local label="$1"
    local source_path="$2"
    local expected="$3"
    local ready_path="$TMP_ROOT/${label}.port"
    local case_path="$TMP_ROOT/${label}.sh"
    start_hanging_listener "$ready_path"
    local port
    port="$(<"$ready_path")"
    write_issue_case "$source_path" "$port" "$case_path"
    local rc=0
    measure_case "$case_path" "$label" || rc=$?
    wait "${LISTENER_PIDS[${#LISTENER_PIDS[@]}-1]}" 2>/dev/null || true
    if [ "$expected" = "ok" ]; then
        if [ "$rc" -ne 0 ]; then
            echo "FAIL: restored gh issue-create guard exceeded the 17s assertion deadline" >&2
            FAILURES=$((FAILURES + 1))
        else
            echo "gh issue create restored: ok"
        fi
    elif [ "$rc" -eq 0 ]; then
        echo "FAIL: removing gh issue-create timeout did not fail the timeout assertion" >&2
        FAILURES=$((FAILURES + 1))
    else
        echo "gh issue create removed: FAILED (timeout assertion)"
    fi
}

run_issue_leader_exit_variant() {
    local label="$1"
    local source_path="$2"
    local expected="$3"
    local case_path="$TMP_ROOT/${label}.sh"
    write_issue_writer_case "$source_path" persistent 18 "$label" "$case_path" \
        "$TMP_ROOT/${label}.captured-bytes" 0
    local measure_output="$TMP_ROOT/${label}.measure.out"
    local rc=0
    measure_case "$case_path" "$label" > "$measure_output" 2>&1 || rc=$?
    tail -n 1 "$measure_output"
    if [ "$expected" = "ok" ]; then
        if [ "$rc" -ne 0 ]; then
            echo "FAIL: restored issue-create call waited for a leader's inherited stdout" >&2
            FAILURES=$((FAILURES + 1))
        else
            echo "gh issue create leader-exit restored: ok"
        fi
    elif [ "$rc" -eq 0 ]; then
        echo "FAIL: pipe mutation did not fail the leader-exit EOF assertion" >&2
        FAILURES=$((FAILURES + 1))
    else
        echo "gh issue create leader-exit pipe mutation: FAILED (EOF assertion)"
    fi
}

run_issue_writer_variant() {
    local label="$1"
    local source_path="$2"
    local mode="$3"
    local expected="$4"
    local case_path="$TMP_ROOT/${label}.sh"
    local observation_path="$TMP_ROOT/${label}.captured-bytes"
    local measure_output="$TMP_ROOT/${label}.measure.out"
    local rc=0 assertion_rc=0 value_assertion_rc=0 observed=""
    write_issue_writer_case "$source_path" "$mode" 2 "$label" "$case_path" \
        "$observation_path" 1
    measure_case "$case_path" "$label" > "$measure_output" 2>&1 || rc=$?
    tail -n 1 "$measure_output"
    if [ -s "$observation_path" ]; then
        observed="$(tr -d '[:space:]' < "$observation_path")"
    fi
    if [ -z "$observed" ] || ! [[ "$observed" =~ ^[0-9]+$ ]] \
        || [ "$observed" -gt "$ISSUE_CAPTURE_READ_LIMIT" ]; then
        assertion_rc=1
    fi
    if ! grep -qF 'returned truncated stdout' "$measure_output" \
        || grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$measure_output"; then
        value_assertion_rc=1
    fi
    if [ "$expected" = "ok" ]; then
        if [ "$rc" -ne 0 ] || [ "$assertion_rc" -ne 0 ] || [ "$value_assertion_rc" -ne 0 ]; then
            echo "FAIL: $mode writer exceeded the capture lookahead or reported its capped value" >&2
            FAILURES=$((FAILURES + 1))
        else
            echo "$mode writer restored: ok (read_bytes=$observed <= $ISSUE_CAPTURE_READ_LIMIT; capped value rejected)"
        fi
    elif [ "$expected" = "truncation" ]; then
        if grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$measure_output"; then
            echo "$mode growth-window truncation mutant: FAILED (self-assertion: capped value was reported)"
        else
            echo "FAIL: removing truncation detection did not report the $mode capped value" >&2
            FAILURES=$((FAILURES + 1))
        fi
    elif [ "$assertion_rc" -eq 0 ]; then
        echo "FAIL: removing the capture bound did not fail the $mode writer assertion" >&2
        FAILURES=$((FAILURES + 1))
    else
        echo "$mode writer cap removed: FAILED (self-assertion: read_bytes=$observed > $ISSUE_CAPTURE_READ_LIMIT)"
    fi
}

MUTATED_NOTIFY="$TMP_ROOT/deploy-no-notify-timeout.sh"
MUTATED_ISSUE="$TMP_ROOT/deploy-no-gh-timeout.sh"
MUTATED_ISSUE_PIPE="$TMP_ROOT/deploy-gh-stdout-pipe.sh"
MUTATED_ISSUE_READ="$TMP_ROOT/deploy-unbounded-issue-read.sh"
MUTATED_ISSUE_VALUE="$TMP_ROOT/deploy-mutant-issue-value.sh"
MUTATED_ISSUE_TRUNCATION="$TMP_ROOT/deploy-no-stdout-truncation.sh"
MUTATED_ISSUE_MERGE="$TMP_ROOT/deploy-merged-issue-streams.sh"
MUTATED_ISSUE_EMPTY_GUARD="$TMP_ROOT/deploy-no-empty-stdout-guard.sh"
MUTATED_ISSUE_RC_GATE="$TMP_ROOT/deploy-no-gh-rc-gate.sh"
python3 - "$DEPLOY_SH" "$MUTATED_NOTIFY" "$MUTATED_ISSUE" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
notify = source.replace(
    "curl -sf --connect-timeout 2 --max-time 15 -X POST",
    "curl -sf -X POST",
    1,
)
issue = source.replace(
    'python3 "$SCRIPT_DIR/ci-timeout.py" 10 gh issue create',
    "gh issue create",
    1,
)
assert notify != source
assert issue != source
Path(sys.argv[2]).write_text(notify)
Path(sys.argv[3]).write_text(issue)
PY
python3 - "$DEPLOY_SH" "$MUTATED_ISSUE_PIPE" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
mutated = source.replace(
    '                    --body-file "$draft_path" > "$tmp_issue_stdout" 2> "$tmp_issue_stderr"; then',
    '                    --body-file "$draft_path" | cat > "$tmp_issue_stdout" 2> "$tmp_issue_stderr"; then',
    1,
)
assert mutated != source
Path(sys.argv[2]).write_text(mutated)
PY
python3 - "$DEPLOY_SH" "$MUTATED_ISSUE_READ" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
needle = 'head -c "$((issue_capture_limit + 1))"'
assert source.count(needle) == 2
mutated = source.replace(needle, "cat")
mutated = mutated.replace(
    '"${issue_stderr:0:issue_capture_limit}"',
    '"$issue_stderr"',
    1,
)
assert mutated != source
Path(sys.argv[2]).write_text(mutated)
PY
python3 - "$DEPLOY_SH" "$MUTATED_ISSUE_VALUE" "$MUTATED_ISSUE_TRUNCATION" \
    "$MUTATED_ISSUE_EMPTY_GUARD" "$MUTATED_ISSUE_RC_GATE" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
value = source.replace(
    'issue_url=$(head -c "$((issue_capture_limit + 1))" "$tmp_issue_stdout" 2>/dev/null; printf x)',
    "issue_url=$(printf '%s' 'MUTANT-NOT-A-URLx')",
    1,
)
truncation = source.replace(
    'if [ "${#issue_url}" -gt "$issue_capture_limit" ]; then',
    'if false; then',
    1,
)
empty_guard = source.replace(
    '''elif issue_url=${issue_url%$'\\n'}; [ -z "$issue_url" ]; then''',
    '''elif issue_url=${issue_url%$'\\n'}; false; then''',
    1,
)
rc_gate = source.replace(
    '                if [ "$rc" -eq 0 ]; then',
    '                if true; then',
    1,
)
assert value != source
assert truncation != source
assert empty_guard != source
assert rc_gate != source
Path(sys.argv[2]).write_text(value)
Path(sys.argv[3]).write_text(truncation)
Path(sys.argv[4]).write_text(empty_guard)
Path(sys.argv[5]).write_text(rc_gate)
PY
python3 - "$DEPLOY_SH" "$MUTATED_ISSUE_MERGE" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
mutated = source.replace(
    '                    --body-file "$draft_path" > "$tmp_issue_stdout" 2> "$tmp_issue_stderr"; then',
    '                    --body-file "$draft_path" > "$tmp_issue_stdout" 2>&1; then',
    1,
)
assert mutated != source
Path(sys.argv[2]).write_text(mutated)
PY

for syntax_path in "$DEPLOY_SH" "$MUTATED_NOTIFY" "$MUTATED_ISSUE" \
    "$MUTATED_ISSUE_PIPE" "$MUTATED_ISSUE_READ" "$MUTATED_ISSUE_VALUE" \
    "$MUTATED_ISSUE_TRUNCATION" "$MUTATED_ISSUE_MERGE" \
    "$MUTATED_ISSUE_EMPTY_GUARD" "$MUTATED_ISSUE_RC_GATE"; do
    if bash -n "$syntax_path"; then
        echo "bash -n rc=0: $(basename "$syntax_path")"
    else
        echo "FAIL: bash -n rejected $(basename "$syntax_path")" >&2
        FAILURES=$((FAILURES + 1))
    fi
done

run_issue_output_variant restored_value "$DEPLOY_SH" valid value
run_issue_output_mutant mutant_value "$MUTATED_ISSUE_VALUE" valid value
run_issue_output_mutant mutant_merge "$MUTATED_ISSUE_MERGE" valid merge
run_issue_output_variant restored_exact "$DEPLOY_SH" exact exact
run_issue_output_variant restored_truncation "$DEPLOY_SH" truncated truncation
run_issue_output_mutant mutant_truncation "$MUTATED_ISSUE_TRUNCATION" truncated truncation
run_issue_output_variant restored_nul_unsupported "$DEPLOY_SH" nul nul_unsupported
run_issue_output_variant restored_empty "$DEPLOY_SH" empty empty
run_issue_output_mutant mutant_empty "$MUTATED_ISSUE_EMPTY_GUARD" empty empty
run_issue_output_variant restored_partial "$DEPLOY_SH" partial invalid
run_issue_output_variant restored_failure "$DEPLOY_SH" failure failure
run_issue_output_mutant mutant_rc "$MUTATED_ISSUE_RC_GATE" failure rc
run_issue_stderr_variant restored_stderr "$DEPLOY_SH" ok 8192
run_issue_stderr_variant restored_exact_stderr "$DEPLOY_SH" exact 4096
run_issue_stderr_variant removed_stderr "$MUTATED_ISSUE_READ" failed 8192

TCP_LISTENER_AVAILABLE=1
TCP_PROBE_READY="$TMP_ROOT/tcp-probe.port"
if ! start_hanging_listener "$TCP_PROBE_READY"; then
    TCP_LISTENER_AVAILABLE=0
    record_skipped_cases 4
    echo "SKIP: loopback TCP bind is unavailable in this restricted runner; " \
        "the same listener mutation cases are exercised when local sockets are permitted"
fi
if [ "$TCP_LISTENER_AVAILABLE" -eq 1 ]; then
    wait "${LISTENER_PIDS[${#LISTENER_PIDS[@]}-1]}" 2>/dev/null || true
    run_notify_variant restored "$DEPLOY_SH" ok
    run_notify_variant removed "$MUTATED_NOTIFY" failed
    run_issue_variant restored_issue "$DEPLOY_SH" ok
    run_issue_variant removed_issue "$MUTATED_ISSUE" failed
fi
run_issue_leader_exit_variant restored_leader_exit "$DEPLOY_SH" ok
run_issue_leader_exit_variant removed_leader_exit "$MUTATED_ISSUE_PIPE" failed
run_issue_writer_variant restored_finite "$DEPLOY_SH" finite ok
run_issue_writer_variant removed_finite "$MUTATED_ISSUE_READ" finite failed
run_issue_writer_variant restored_persistent "$DEPLOY_SH" persistent ok
run_issue_writer_variant removed_persistent "$MUTATED_ISSUE_READ" persistent failed
run_issue_writer_variant restored_growth_window "$DEPLOY_SH" race ok
run_issue_writer_variant mutant_growth_window "$MUTATED_ISSUE_TRUNCATION" race truncation

python3 - "$RELAY_PY" "$MATRIX_PY" <<'PY'
import ast
import contextlib
import importlib.util
import io
import os
import sys
import tempfile
from pathlib import Path

source_path = Path(sys.argv[1])
matrix_path = Path(sys.argv[2])
source = source_path.read_text()


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_matrix(cell_module, name: str, path=None):
    sys.modules["run_tui_relay"] = cell_module
    return load(path or matrix_path, name)


def probe(module, matrix_module) -> None:
    names = (
        "AGENTDESK_E2E_TURN_START_TIMEOUT_S",
        "AGENTDESK_E2E_FINAL_REFETCH_INTERVAL_S",
        "AGENTDESK_E2E_FINAL_REFETCHES",
    )
    old_env = {name: os.environ.get(name) for name in names}
    old_argv = sys.argv
    try:
        for name in names:
            os.environ[name] = "1e12"
        sys.argv = [
            str(source_path),
            "--cell",
            "claude-pipe",
            "--channel-id",
            "bounded-5274",
        ]
        with contextlib.redirect_stderr(io.StringIO()) as warnings:
            args = module.parse_args()
        assert args.turn_start_timeout_s == 180.0, args.turn_start_timeout_s
        assert args.final_refetches == 3, args.final_refetches
        assert args.final_refetch_interval_s == 60.0, args.final_refetch_interval_s
        warning_text = warnings.getvalue()
        assert warning_text.count("WARNING:") == 3, warning_text
        for name in names:
            assert name in warning_text, (name, warning_text)

        sys.argv = ["matrix"]
        with contextlib.redirect_stderr(io.StringIO()) as matrix_warnings:
            matrix_args = matrix_module.parse_args()
        assert matrix_args.turn_start_timeout_s == 180.0, matrix_args.turn_start_timeout_s
        assert matrix_args.final_refetches == 3, matrix_args.final_refetches
        assert matrix_args.final_refetch_interval_s == 60.0, matrix_args.final_refetch_interval_s
        matrix_warning_text = matrix_warnings.getvalue()
        assert matrix_warning_text.count("WARNING:") == 3, matrix_warning_text
        for name in names:
            assert name in matrix_warning_text, (name, matrix_warning_text)
    finally:
        sys.argv = old_argv
        for name, value in old_env.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


def probe_nonfinite(module) -> None:
    for raw in ("nan", "inf"):
        with contextlib.redirect_stderr(io.StringIO()) as warning:
            bounded = module._bounded_value(
                raw,
                source="nonfinite-probe",
                default=180.0,
                minimum=1.0,
                maximum=180.0,
            )
        assert bounded == 180.0, (raw, bounded)
        assert "is invalid; clamped to 180.0" in warning.getvalue(), warning.getvalue()


def probe_cli(module, matrix_module=None) -> None:
    """A huge explicit CLI value must be clamped independently of env defaults."""

    names = (
        "AGENTDESK_E2E_TURN_START_TIMEOUT_S",
        "AGENTDESK_E2E_FINAL_REFETCH_INTERVAL_S",
        "AGENTDESK_E2E_FINAL_REFETCHES",
    )
    old_env = {name: os.environ.get(name) for name in names}
    old_argv = sys.argv
    try:
        os.environ.update(
            {
                "AGENTDESK_E2E_TURN_START_TIMEOUT_S": "180",
                "AGENTDESK_E2E_FINAL_REFETCH_INTERVAL_S": "1",
                "AGENTDESK_E2E_FINAL_REFETCHES": "2",
            }
        )
        sys.argv = [
            str(source_path),
            "--cell",
            "claude-pipe",
            "--channel-id",
            "bounded-5274",
            "--turn-start-timeout-s",
            "1e12",
        ]
        args = module.parse_args()
        assert args.turn_start_timeout_s == 180.0, args.turn_start_timeout_s
        if matrix_module is not None:
            sys.argv = ["matrix", "--turn-start-timeout-s", "1e12"]
            matrix_args = matrix_module.parse_args()
            assert matrix_args.turn_start_timeout_s == 180.0, matrix_args.turn_start_timeout_s
    finally:
        sys.argv = old_argv
        for name, value in old_env.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


with tempfile.TemporaryDirectory(prefix="agentdesk-e1-mutation-") as temp_dir:
    temp_path = Path(temp_dir) / "run_tui_relay.py"
    matrix_temp_path = Path(temp_dir) / "run_multi_provider_matrix.py"
    restored_module = load(source_path, "run_tui_relay")
    restored_matrix = load_matrix(restored_module, "matrix_5274_restored")
    probe(restored_module, restored_matrix)
    probe_nonfinite(restored_module)
    probe_cli(restored_module, restored_matrix)
    print("E-1/matrix guards restored: ok (1e12/nan/inf inputs clamped; warnings observed)")

    nonfinite_source = source.replace(
        '''        if not math.isfinite(parsed_float):
            raise ValueError("non-finite")
''',
        "",
        1,
    )
    assert nonfinite_source != source
    compile(nonfinite_source, str(temp_path), "exec")
    temp_path.write_text(nonfinite_source)
    try:
        nonfinite_mutant = load(temp_path, "run_tui_relay_nonfinite_removed")
        probe_nonfinite(nonfinite_mutant)
    except (AssertionError, ValueError) as error:
        print(f"math.isfinite guard removed: FAILED (self-assertion: {error})")
    else:
        raise SystemExit("removing math.isfinite did not fail its non-finite assertion")

    tui_cli_source = source.replace(
        '''    args.turn_start_timeout_s = _bounded_value(
        args.turn_start_timeout_s,
        source="--turn-start-timeout-s",
        default=180.0,
        minimum=1.0,
        maximum=E2E_TURN_START_TIMEOUT_MAX_S,
    )
''',
        "    args.turn_start_timeout_s = args.turn_start_timeout_s\n",
        1,
    )
    assert tui_cli_source != source
    temp_path.write_text(tui_cli_source)
    try:
        tui_cli_mutant = load(temp_path, "run_tui_relay_cli_removed")
        probe_cli(tui_cli_mutant)
    except (AssertionError, ValueError) as error:
        print(f"E-1 TUI CLI clamp removed: FAILED (self-assertion: {error})")
    else:
        raise SystemExit("removing the TUI CLI clamp did not fail its assertion")

    matrix_source = matrix_path.read_text()
    matrix_cli_source = matrix_source.replace(
        '''    args.turn_start_timeout_s = cell_driver._bounded_value(  # noqa: SLF001
        args.turn_start_timeout_s,
        source="--turn-start-timeout-s",
        default=180.0,
        minimum=1.0,
        maximum=cell_driver.E2E_TURN_START_TIMEOUT_MAX_S,
    )
''',
        "    args.turn_start_timeout_s = args.turn_start_timeout_s\n",
        1,
    )
    assert matrix_cli_source != matrix_source
    matrix_temp_path.write_text(matrix_cli_source)
    try:
        matrix_cli_mutant = load_matrix(
            restored_module, "matrix_cli_removed", matrix_temp_path
        )
        probe_cli(restored_module, matrix_cli_mutant)
    except (AssertionError, ValueError) as error:
        print(f"E-1 matrix CLI clamp removed: FAILED (self-assertion: {error})")
    else:
        raise SystemExit("removing the matrix CLI clamp did not fail its assertion")

    tree = ast.parse(source)
    bounded = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_bounded_value"
    )
    lines = source.splitlines(keepends=True)
    mutated = (
        lines[: bounded.lineno - 1]
        + [
            "def _bounded_value(raw: object, **_kwargs: object) -> object:\n",
            "    return float(raw)\n",
        ]
        + lines[bounded.end_lineno :]
    )
    temp_path.write_text("".join(mutated))
    try:
        mutated_module = load(temp_path, "run_tui_relay")
        probe(mutated_module, load_matrix(mutated_module, "matrix_5274_removed"))
    except (AssertionError, ValueError) as error:
        print(f"E-1 clamp guard removed: FAILED (self-assertion: {error})")
    else:
        raise SystemExit("removing the shared E-1 clamp did not fail its assertions")
PY

if [ "$FAILURES" -ne 0 ]; then
    echo "test_deploy_bounded_calls_5274: $FAILURES assertion(s) failed" >&2
    exit 1
fi
if [ "$SKIPPED_CASES" -gt 0 ]; then
    echo "test_deploy_bounded_calls_5274: incomplete with skipped=$SKIPPED_CASES (not evaluated); remaining assertions passed" >&2
    exit 2
fi
echo "test_deploy_bounded_calls_5274: all assertions passed; skipped=0"
