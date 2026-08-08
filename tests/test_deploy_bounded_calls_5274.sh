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
    local real_awk real_cat real_wc
    real_awk="$(command -v awk)"
    real_cat="$(command -v cat)"
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
        cat > "$bin/awk" <<EOF
#!/usr/bin/env bash
set -euo pipefail
"$real_cat" > "$TMP_ROOT/${label}.capture"
"$real_wc" -c < "$TMP_ROOT/${label}.capture" > "$observation_path"
"$real_awk" "\$@" "$TMP_ROOT/${label}.capture"
EOF
        chmod +x "$bin/awk"
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
    local rc=0
    measure_case "$case_path" "$label" || rc=$?
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
    local rc=0 assertion_rc=0 observed=""
    write_issue_writer_case "$source_path" "$mode" 2 "$label" "$case_path" \
        "$observation_path" 1
    measure_case "$case_path" "$label" || rc=$?
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
    '                    --body-file "$draft_path" > "$tmp_issue_out" 2>&1; then',
    '                    --body-file "$draft_path" | cat > "$tmp_issue_out" 2>&1; then',
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


def load_matrix(cell_module, name: str):
    sys.modules["run_tui_relay"] = cell_module
    return load(matrix_path, name)


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


with tempfile.TemporaryDirectory(prefix="agentdesk-e1-mutation-") as temp_dir:
    temp_path = Path(temp_dir) / "run_tui_relay.py"
    restored_module = load(source_path, "run_tui_relay")
    probe(restored_module, load_matrix(restored_module, "matrix_5274_restored"))
    print("E-1/matrix guards restored: ok (1e12 inputs clamped; warnings observed at parse)")

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
