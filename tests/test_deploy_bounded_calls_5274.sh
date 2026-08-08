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

if ! grep -Fq 'curl -sf --connect-timeout 2 --max-time 15 -X POST' "$DEPLOY_SH"; then
    echo "FAIL: _notify_channel is missing the fixed connect/total timeout" >&2
    FAILURES=$((FAILURES + 1))
fi
if ! grep -Fq 'python3 "$SCRIPT_DIR/ci-timeout.py" 10 gh issue create' "$DEPLOY_SH"; then
    echo "FAIL: confirmed-mode gh issue create is missing the repository timeout runner" >&2
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
if [ "$output_mode" = truncated ]; then
    printf '%s\\n' 'https://example.invalid/issues/5274'
    head -c 8192 /dev/zero
else
    printf '%s\\n' 'https://example.invalid/issues/5274'
fi
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
    local case_path="$TMP_ROOT/${label}.sh"
    local bin="$TMP_ROOT/${label}-bin"
    mkdir -p "$TMP_ROOT/release/logs" "$bin"
    cat > "$bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
head -c 8192 /dev/zero >&2
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
    local rc=0
    write_issue_output_case "$source_path" "$output_mode" "$label" "$case_path"
    bash "$case_path" > "$output_path" 2>&1 || rc=$?
    cat "$output_path"
    if [ "$expected" = value ] && [ "$rc" -eq 0 ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://example.invalid/issues/5274' "$output_path" \
        && ! grep -qF 'https://stderr.example.invalid/first' "$output_path"; then
        echo "issue_url value restored: ok (stdout URL survived stderr noise)"
    elif [ "$expected" = truncation ] && [ "$rc" -eq 0 ] \
        && grep -qF 'returned truncated stdout' "$output_path" \
        && ! grep -qF 'Post-deploy smoke issue created (confirmed mode):' "$output_path"; then
        echo "stdout truncation detection restored: ok (no URL reported)"
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
        && ! grep -qF 'https://example.invalid/issues/5274' "$output_path"; then
        echo "issue_url value mutant: FAILED (self-assertion: fixture URL was not reported)"
    elif [ "$mutant_kind" = truncation ] \
        && grep -qF 'Post-deploy smoke issue created (confirmed mode): https://example.invalid/issues/5274' "$output_path"; then
        echo "stdout truncation mutant: FAILED (self-assertion: truncated URL was reported)"
    elif [ "$mutant_kind" = merge ] \
        && grep -qF 'https://stderr.example.invalid/first' "$output_path"; then
        echo "stdout/stderr merge mutant: FAILED (self-assertion: stderr URL leaked into issue_url)"
    else
        echo "FAIL: $mutant_kind mutant survived its value/truncation assertion (rc=$rc)" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

run_issue_stderr_variant() {
    local label="$1"
    local source_path="$2"
    local expected="$3"
    local case_path="$TMP_ROOT/${label}.sh"
    local output_path="$TMP_ROOT/${label}.out"
    local evidence_path="$TMP_ROOT/${label}-evidence.log"
    local marker='[gh stderr truncated at 4096 bytes]'
    local max_bytes=$((ISSUE_CAPTURE_LIMIT + ${#marker} + 2))
    local actual_bytes=0 rc=0
    write_issue_stderr_case "$source_path" "$label" "$case_path"
    bash "$case_path" > "$output_path" 2>&1 || rc=$?
    actual_bytes=$(stat -f%z "$evidence_path" 2>/dev/null || stat -c%s "$evidence_path")
    if [ "$expected" = ok ] && [ "$rc" -eq 0 ] \
        && grep -aFq "$marker" "$evidence_path" \
        && [ "$actual_bytes" -le "$max_bytes" ]; then
        echo "stderr evidence cap restored: ok (bytes=$actual_bytes <= $max_bytes)"
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
    local real_head real_cat real_stat real_wc
    real_head="$(command -v head)"
    real_cat="$(command -v cat)"
    real_stat="$(command -v stat)"
    real_wc="$(command -v wc)"
    mkdir -p "$TMP_ROOT/release/logs" "$bin"
    cat > "$bin/gh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# The gh leader exits after its child has written the initial payload. The child
# therefore keeps gh's stdout descriptor open after the leader is gone.
(
    python3 - "$ready_path" "$mode" "$linger_s" <<'PY'
import sys
import time
from pathlib import Path

ready_path = Path(sys.argv[1])
mode = sys.argv[2]
linger_s = float(sys.argv[3])
payload = b"https://example.invalid/issues/5274\\n" + (b"f" * 8192)
sys.stdout.buffer.write(payload)
sys.stdout.buffer.flush()
ready_path.write_text("ready", encoding="ascii")
if mode == "finite":
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
    "$real_cat" "\$@" > "$TMP_ROOT/${label}.capture"
    "$real_wc" -c < "$TMP_ROOT/${label}.capture" > "$observation_path"
    "$real_cat" "$TMP_ROOT/${label}.capture"
else
    exec "$real_cat" "\$@"
fi
EOF
        chmod +x "$bin/cat"
        cat > "$bin/stat" <<EOF
#!/usr/bin/env bash
set -euo pipefail
target="\${@: -1}"
if [[ "\$target" == *agentdesk-issue-stdout* ]]; then
    printf '%s\\n' 1
else
    exec "$real_stat" "\$@"
fi
EOF
        chmod +x "$bin/stat"
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
            echo "FAIL: restored gh issue-create guard exceeded its 10s command budget" >&2
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
    local rc=0 assertion_rc=0 observed=""
    write_issue_writer_case "$source_path" "$mode" 2 "$label" "$case_path" \
        "$observation_path" 1
    measure_case "$case_path" "$label" > "$measure_output" 2>&1 || rc=$?
    tail -n 1 "$measure_output"
    if [ -s "$observation_path" ]; then
        observed="$(tr -d '[:space:]' < "$observation_path")"
    fi
    if [ -z "$observed" ] || ! [[ "$observed" =~ ^[0-9]+$ ]] \
        || [ "$observed" -gt "$ISSUE_CAPTURE_LIMIT" ]; then
        assertion_rc=1
    fi
    if [ "$expected" = "ok" ]; then
        if [ "$rc" -ne 0 ] || [ "$assertion_rc" -ne 0 ]; then
            echo "FAIL: $mode writer exceeded the ${ISSUE_CAPTURE_LIMIT}-byte capture bound" >&2
            FAILURES=$((FAILURES + 1))
        else
            echo "$mode writer restored: ok (read_bytes=$observed <= $ISSUE_CAPTURE_LIMIT)"
        fi
    elif [ "$assertion_rc" -eq 0 ]; then
        echo "FAIL: removing the capture bound did not fail the $mode writer assertion" >&2
        FAILURES=$((FAILURES + 1))
    else
        echo "$mode writer cap removed: FAILED (self-assertion: read_bytes=$observed > $ISSUE_CAPTURE_LIMIT)"
    fi
}

MUTATED_NOTIFY="$TMP_ROOT/deploy-no-notify-timeout.sh"
MUTATED_ISSUE="$TMP_ROOT/deploy-no-gh-timeout.sh"
MUTATED_ISSUE_PIPE="$TMP_ROOT/deploy-gh-stdout-pipe.sh"
MUTATED_ISSUE_READ="$TMP_ROOT/deploy-unbounded-issue-read.sh"
MUTATED_ISSUE_VALUE="$TMP_ROOT/deploy-mutant-issue-value.sh"
MUTATED_ISSUE_TRUNCATION="$TMP_ROOT/deploy-no-stdout-truncation.sh"
MUTATED_ISSUE_MERGE="$TMP_ROOT/deploy-merged-issue-streams.sh"
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
needle = 'head -c "$issue_capture_limit"'
assert source.count(needle) == 2
mutated = source.replace(needle, "cat")
assert mutated != source
Path(sys.argv[2]).write_text(mutated)
PY
python3 - "$DEPLOY_SH" "$MUTATED_ISSUE_VALUE" "$MUTATED_ISSUE_TRUNCATION" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text()
value = source.replace(
    'issue_url=$(head -c "$issue_capture_limit" "$tmp_issue_stdout")',
    "issue_url=$(printf '%s\\n' 'MUTANT-NOT-A-URL')",
    1,
)
truncation = source.replace(
    'if [ "${issue_stdout_bytes:-0}" -ge "$issue_capture_limit" ] 2>/dev/null; then',
    'if false; then',
    1,
)
assert value != source
assert truncation != source
Path(sys.argv[2]).write_text(value)
Path(sys.argv[3]).write_text(truncation)
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
    "$MUTATED_ISSUE_TRUNCATION" "$MUTATED_ISSUE_MERGE"; do
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
run_issue_output_variant restored_truncation "$DEPLOY_SH" truncated truncation
run_issue_output_mutant mutant_truncation "$MUTATED_ISSUE_TRUNCATION" truncated truncation
run_issue_stderr_variant restored_stderr "$DEPLOY_SH" ok
run_issue_stderr_variant removed_stderr "$MUTATED_ISSUE_READ" failed

TCP_LISTENER_AVAILABLE=1
TCP_PROBE_READY="$TMP_ROOT/tcp-probe.port"
if ! start_hanging_listener "$TCP_PROBE_READY"; then
    TCP_LISTENER_AVAILABLE=0
    SKIPPED_CASES=$((SKIPPED_CASES + 4))
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
    probe_cli(restored_module, restored_matrix)
    print("E-1/matrix guards restored: ok (1e12 inputs clamped; warnings observed at parse)")

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
    echo "test_deploy_bounded_calls_5274: completed with skipped=$SKIPPED_CASES (not evaluated); remaining assertions passed"
else
    echo "test_deploy_bounded_calls_5274: all assertions passed; skipped=0"
fi
