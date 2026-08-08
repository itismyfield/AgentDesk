#!/usr/bin/env bash
# Mutation proof for #5274 slice A: notification, confirmed-mode issue creation,
# and E-1 operator overrides must all return through a fixed bound.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
RELAY_PY="$REPO_ROOT/scripts/e2e/run_tui_relay.py"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-bounded-calls-5274.XXXXXX")
LISTENER_PIDS=()
FAILURES=0

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
    # 16.5s. The production 15s cap therefore has a short, observable margin;
    # a removed cap remains alive when the test's 15.8s assertion deadline fires.
    python3 - "$ready_path" 2>/dev/null <<'PY' &
import socket
import sys
import time
from pathlib import Path

ready_path = Path(sys.argv[1])
deadline = time.monotonic() + 16.5
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
assertion_deadline_s = 15.8
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

MUTATED_NOTIFY="$TMP_ROOT/deploy-no-notify-timeout.sh"
MUTATED_ISSUE="$TMP_ROOT/deploy-no-gh-timeout.sh"
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

TCP_LISTENER_AVAILABLE=1
TCP_PROBE_READY="$TMP_ROOT/tcp-probe.port"
if ! start_hanging_listener "$TCP_PROBE_READY"; then
    TCP_LISTENER_AVAILABLE=0
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

python3 - "$RELAY_PY" <<'PY'
import ast
import contextlib
import importlib.util
import io
import os
import sys
import tempfile
from pathlib import Path

source_path = Path(sys.argv[1])
source = source_path.read_text()


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def probe(module) -> None:
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
            settings = module._final_refetch_settings()
        assert args.turn_start_timeout_s == 180.0, args.turn_start_timeout_s
        assert settings == (2, 60.0), settings
        warning_text = warnings.getvalue()
        assert warning_text.count("WARNING:") == 3, warning_text
        for name in names:
            assert name in warning_text, (name, warning_text)
    finally:
        sys.argv = old_argv
        for name, value in old_env.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


with tempfile.TemporaryDirectory(prefix="agentdesk-e1-mutation-") as temp_dir:
    temp_path = Path(temp_dir) / "run_tui_relay.py"
    probe(load(source_path, "relay_5274_restored"))
    print("E-1 guards restored: ok (1e12 inputs clamped; three warnings observed)")

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
        probe(load(temp_path, "relay_5274_removed"))
    except (AssertionError, ValueError) as error:
        print(f"E-1 clamp guard removed: FAILED (self-assertion: {error})")
    else:
        raise SystemExit("removing the shared E-1 clamp did not fail its assertions")
PY

if [ "$FAILURES" -ne 0 ]; then
    echo "test_deploy_bounded_calls_5274: $FAILURES assertion(s) failed" >&2
    exit 1
fi
echo "test_deploy_bounded_calls_5274: all assertions passed"
