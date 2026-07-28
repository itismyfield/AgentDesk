#!/usr/bin/env bash
# Behavioral regression coverage for #4902 routine asset transactions.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../scripts/routine-asset-surface.sh
. "$REPO_ROOT/scripts/routine-asset-surface.sh"

# The checked-in loader root itself is part of the contract: every tracked JS
# file under routines/ must be a QuickJS registration entrypoint, and each
# migrated helper must exist only on the sibling helper surface. This catches a
# future helper accidentally being moved back under the recursive loader root.
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    if [ -e "$REPO_ROOT/routines/$helper_ref" ]; then
        echo "FAIL: legacy helper returned to QuickJS loader root: $helper_ref" >&2
        exit 1
    fi
    if [ ! -f "$REPO_ROOT/routine-helpers/$helper_ref" ]; then
        echo "FAIL: migrated helper missing from sibling surface: $helper_ref" >&2
        exit 1
    fi
done
while IFS= read -r routine_ref; do
    if ! grep -Fq 'agentdesk.routines.register' "$REPO_ROOT/$routine_ref"; then
        echo "FAIL: tracked routines/ JS is not a QuickJS entrypoint: $routine_ref" >&2
        exit 1
    fi
done < <(git -C "$REPO_ROOT" ls-files 'routines/*.js' 'routines/**/*.js')

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-routine-assets.XXXXXX")
trap 'adk_release_routine_asset_lock 2>/dev/null || true; rm -rf "$TMP_ROOT"' EXIT
ADK_ROUTINE_VALIDATOR_BINARY="$TMP_ROOT/fake-routine-validator"
printf '%s\n' \
    '#!/bin/bash' \
    'set -euo pipefail' \
    '[ "${1:-}" = validate-routines ]' \
    '[ "${2:-}" = --root ]' \
    'root="${3:-}"' \
    '[ "${4:-}" = --runtime-root ]' \
    'runtime_root="${5:-}"' \
    '[ "$root" = "$runtime_root/routines" ]' \
    '[ -d "$root" ] && [ -d "$runtime_root/routine-helpers" ]' \
    > "$ADK_ROUTINE_VALIDATOR_BINARY"
chmod +x "$ADK_ROUTINE_VALIDATOR_BINARY"

fail_test() {
    echo "FAIL: $*" >&2
    exit 1
}

wait_for_test_file() {
    local path="$1"
    local attempts=0

    while [ ! -e "$path" ]; do
        [ "$attempts" -lt 500 ] \
            || fail_test "timed out waiting for test synchronization file: $path"
        sleep 0.01
        attempts=$((attempts + 1))
    done
}

launch_lock_contender() {
    local lock_file="$1"
    local result_file="$2"
    local release_file="$3"
    local ready_file="${result_file}.ready"

    bash -c '
        set -u
        . "$1"
        : > "$5"
        if adk_acquire_routine_asset_lock "$2" 0 >/dev/null 2>&1; then
            printf "won:%s\n" "$ADK_ROUTINE_ASSET_LOCK_TOKEN" > "$3"
            while [ ! -e "$4" ]; do sleep 0.01; done
            adk_release_routine_asset_lock
        else
            printf "lost\n" > "$3"
        fi
    ' _ "$REPO_ROOT/scripts/routine-asset-surface.sh" \
        "$lock_file" "$result_file" "$release_file" "$ready_file" &
    LAST_LOCK_CONTENDER_PID=$!
}

hold_test_lock_guard() {
    local guard_path="$1"
    local ready_file="$2"
    local release_file="$3"

    python3 - "$guard_path" "$ready_file" "$release_file" <<'PY' &
import fcntl
import os
import sys
import time

guard_path, ready_file, release_file = sys.argv[1:]
guard = os.open(guard_path, os.O_RDWR | os.O_CREAT, 0o600)
try:
    fcntl.flock(guard, fcntl.LOCK_EX)
    with open(ready_file, "w", encoding="utf-8"):
        pass
    while not os.path.exists(release_file):
        time.sleep(0.01)
finally:
    os.close(guard)
PY
    TEST_GUARD_PID=$!
}

hold_test_peer_claim_guard() {
    local lock_file="$1"
    local incoming="$2"
    local ready_file="$3"
    local publish_file="$4"

    python3 - "$lock_file" "$incoming" "$ready_file" "$publish_file" <<'PY' &
import fcntl
import json
import os
import sys
import tempfile
import time

lock_file, incoming, ready_file, publish_file = sys.argv[1:]
guard_path = lock_file + ".d.guard"
record_path = lock_file + ".d"
guard = os.open(guard_path, os.O_RDWR | os.O_CREAT, 0o600)
try:
    fcntl.flock(guard, fcntl.LOCK_EX)
    with open(ready_file, "w", encoding="utf-8"):
        pass
    while not os.path.exists(publish_file):
        time.sleep(0.01)
    record = {
        "format": 1,
        "identity": "deterministic-test-receiver",
        "pid": os.getpid(),
        "token": "deterministic.receiver.token",
    }
    fd, temporary = tempfile.mkstemp(
        prefix=os.path.basename(record_path) + ".replace.",
        dir=os.path.dirname(record_path),
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(record, handle, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, record_path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
    claimed = os.path.join(incoming, ".claimed")
    claim_fd = os.open(claimed, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(claim_fd, "w", encoding="utf-8") as handle:
        handle.write("deterministic.receiver.token\n")
        handle.flush()
        os.fsync(handle.fileno())
finally:
    os.close(guard)
PY
    TEST_GUARD_PID=$!
}

seed_required_helpers() {
    local helper_root="$1"
    local generation="$2"
    local helper_ref

    mkdir -p "$helper_root/monitoring"
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        printf '%s:%s\n' "$generation" "$helper_ref" > "$helper_root/$helper_ref"
    done
    printf '%s\n' "$generation" > "$helper_root/generation-marker"
}

write_quickjs_routine() {
    local path="$1"
    local generation="$2"

    mkdir -p "$(dirname "$path")"
    printf 'agentdesk.routines.register({ name: "%s", tick() { return { action: "complete" }; } });\n' \
        "$generation" > "$path"
}

seed_source() {
    local root="$1"
    local generation="$2"

    seed_required_helpers "$root/routine-helpers" "$generation"
    write_quickjs_routine "$root/routines/monitoring/bundled.js" "$generation"
    printf '%s\n' "$generation" > "$root/routines/generation-marker"
}

seed_live() {
    local root="$1"
    local generation="$2"

    seed_required_helpers "$root/routine-helpers" "$generation"
    write_quickjs_routine "$root/routines/monitoring/bundled.js" "$generation"
    printf '%s\n' "$generation" > "$root/routines/generation-marker"
}

acquire_runtime_lock() {
    local runtime_root="$1"
    CURRENT_LOCK="$runtime_root/runtime/deploy-release.lock"
    adk_acquire_routine_asset_lock "$CURRENT_LOCK" 0
}

begin_staged_transaction() {
    local source_root="$1"
    local runtime_root="$2"

    CURRENT_TXN="$(
        adk_begin_routine_asset_transaction "$runtime_root" "$CURRENT_LOCK"
    )"
    adk_stage_routines "$source_root" "$runtime_root" "$CURRENT_TXN" >/dev/null
    adk_stage_routine_helpers "$source_root" "$runtime_root" "$CURRENT_TXN" >/dev/null
}

release_runtime_lock() {
    adk_release_routine_asset_lock
    CURRENT_LOCK=""
}

CURRENT_LOCK=""
CURRENT_TXN=""

# Unique stages preserve operator assets, exact tombstones, and root mode.
SOURCE_ROOT="$TMP_ROOT/repo-v1"
RUNTIME_ROOT="$TMP_ROOT/release-v0"
seed_source "$SOURCE_ROOT" 'v1'
seed_live "$RUNTIME_ROOT" 'v0'
write_quickjs_routine \
    "$SOURCE_ROOT/routines/monitoring/bundled-v1-only.js" 'v1-only'
printf 'v1-only helper\n' \
    > "$SOURCE_ROOT/routine-helpers/monitoring/bundled-v1-only.py"
# Make different same-size source/live bytes share mtimes so rsync's default
# quick-check cannot accidentally satisfy this authoritative-overlay regression.
touch -r "$RUNTIME_ROOT/routines/generation-marker" \
    "$SOURCE_ROOT/routines/generation-marker"
touch -r "$RUNTIME_ROOT/routines/monitoring/bundled.js" \
    "$SOURCE_ROOT/routines/monitoring/bundled.js"
touch -r "$RUNTIME_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
    "$SOURCE_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
printf 'operator helper\n' > "$RUNTIME_ROOT/routine-helpers/operator-private.py"
write_quickjs_routine "$RUNTIME_ROOT/routines/monitoring/operator.js" 'operator'
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    mkdir -p "$(dirname "$RUNTIME_ROOT/routines/$helper_ref")"
    printf 'legacy helper\n' > "$RUNTIME_ROOT/routines/$helper_ref"
done
chmod 0710 "$RUNTIME_ROOT/routine-helpers"
acquire_runtime_lock "$RUNTIME_ROOT"
begin_staged_transaction "$SOURCE_ROOT" "$RUNTIME_ROOT"

LEGACY_ROUTINE_STAGE="$RUNTIME_ROOT/routines.new"
[ "$CURRENT_TXN/staged/release-root/routines" != "$LEGACY_ROUTINE_STAGE" ] \
    && [ -d "$CURRENT_TXN/staged/release-root/routines" ] \
    && [ -d "$CURRENT_TXN/staged/release-root/routine-helpers" ] \
    || fail_test 'transaction did not use unique owned stage paths'
[ -f "$CURRENT_TXN/staged/release-root/routine-helpers/operator-private.py" ] \
    || fail_test 'helper staging erased an operator-private asset'
[ -f "$CURRENT_TXN/staged/release-root/routines/monitoring/operator.js" ] \
    || fail_test 'routine staging erased an operator-private entrypoint'
cmp "$SOURCE_ROOT/routines/generation-marker" \
    "$CURRENT_TXN/staged/release-root/routines/generation-marker" >/dev/null \
    && cmp "$SOURCE_ROOT/routines/monitoring/bundled.js" \
        "$CURRENT_TXN/staged/release-root/routines/monitoring/bundled.js" >/dev/null \
    && cmp "$SOURCE_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
        "$CURRENT_TXN/staged/release-root/routine-helpers/monitoring/weekly_churn_audit.py" >/dev/null \
    || fail_test 'source overlay lost authoritative bytes at equal size and mtime'
[ "$(_adk_path_mode "$CURRENT_TXN/staged/release-root/routine-helpers")" = \
    "$(_adk_path_mode "$RUNTIME_ROOT/routine-helpers")" ] \
    || fail_test 'staged helper root did not preserve live mode'
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    [ ! -e "$CURRENT_TXN/staged/release-root/routines/$helper_ref" ] \
        || fail_test "legacy helper survived exact tombstone: $helper_ref"
done

if adk_commit_routine_asset_transaction_forward \
    "$RUNTIME_ROOT" "$CURRENT_TXN" >/dev/null 2>&1; then
    fail_test 'lexical staging state was accepted as fail-forward authority'
fi
[ "$(adk_routine_asset_transaction_phase "$RUNTIME_ROOT" "$CURRENT_TXN")" = \
    'staging' ] \
    && [ "$(<"$RUNTIME_ROOT/routines/generation-marker")" = 'v0' ] \
    || fail_test 'rejected staging fail-forward mutated live routine state'

REJECTING_VALIDATOR="$TMP_ROOT/rejecting-routine-validator"
printf '%s\n' '#!/bin/sh' 'exit 19' > "$REJECTING_VALIDATOR"
chmod +x "$REJECTING_VALIDATOR"
if adk_promote_routine_asset_transaction \
    "$RUNTIME_ROOT" "$CURRENT_TXN" "$REJECTING_VALIDATOR" \
    >/dev/null 2>&1; then
    fail_test 'candidate runtime rejection was ignored before asset promotion'
fi
[ "$(adk_routine_asset_transaction_phase "$RUNTIME_ROOT" "$CURRENT_TXN")" = \
    'staging' ] \
    && [ "$(<"$RUNTIME_ROOT/routines/generation-marker")" = 'v0' ] \
    || fail_test 'candidate rejection mutated live assets or transaction phase'

adk_promote_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
[ "$(<"$RUNTIME_ROOT/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$RUNTIME_ROOT/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$RUNTIME_ROOT/routine-helpers/generation-marker")" = 'v1' ] \
    || fail_test 'paired promotion did not retain the exact v0 rollback generation'
adk_commit_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
[ ! -e "$RUNTIME_ROOT/routines.old" ] \
    && [ ! -e "$RUNTIME_ROOT/routine-helpers.old" ] \
    && [ ! -e "$RUNTIME_ROOT/runtime/routine-assets.active" ] \
    || fail_test 'healthy commit retained transaction state'
release_runtime_lock

# The managed inventory written by v1 is authoritative for removals in v2:
# files formerly shipped by the repository disappear, while files absent from
# that inventory remain operator-owned and survive the preserve/overlay stage.
MANAGED_V2_SOURCE="$TMP_ROOT/repo-v2"
seed_source "$MANAGED_V2_SOURCE" 'v2'
acquire_runtime_lock "$RUNTIME_ROOT"
begin_staged_transaction "$MANAGED_V2_SOURCE" "$RUNTIME_ROOT"
[ ! -e "$CURRENT_TXN/staged/release-root/routines/monitoring/bundled-v1-only.js" ] \
    && [ ! -e "$CURRENT_TXN/staged/release-root/routine-helpers/monitoring/bundled-v1-only.py" ] \
    && [ -f "$CURRENT_TXN/staged/release-root/routines/monitoring/operator.js" ] \
    && [ -f "$CURRENT_TXN/staged/release-root/routine-helpers/operator-private.py" ] \
    || fail_test 'v2 staging did not remove old managed files and preserve operator files'
adk_promote_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
adk_commit_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
[ "$(<"$RUNTIME_ROOT/routines/generation-marker")" = 'v2' ] \
    && [ ! -e "$RUNTIME_ROOT/routines/monitoring/bundled-v1-only.js" ] \
    && [ ! -e "$RUNTIME_ROOT/routine-helpers/monitoring/bundled-v1-only.py" ] \
    && [ -f "$RUNTIME_ROOT/routines/monitoring/operator.js" ] \
    && [ -f "$RUNTIME_ROOT/routine-helpers/operator-private.py" ] \
    || fail_test 'v2 commit retained removed managed files or erased operator files'
release_runtime_lock

# Lock publication and stale-owner replacement are serialized by the guard:
# at the boundary no partial owner record is visible and exactly one live
# contender can replace either an absent record or a dead owner's record.
run_competing_lock_case() {
    local case_name="$1"
    local lock_file="$2"
    local lock_record="${lock_file}.d"
    local sync_root="$TMP_ROOT/lock-race-$case_name"
    local first_result="$sync_root/first.result"
    local second_result="$sync_root/second.result"
    local guard_ready="$sync_root/guard.ready"
    local guard_release="$sync_root/guard.release"
    local owner_release="$sync_root/owner.release"
    local first_pid
    local second_pid
    local guard_pid
    local winners=0
    local result_file

    mkdir -p "$sync_root" "$(dirname "$lock_file")"
    hold_test_lock_guard "${lock_record}.guard" "$guard_ready" "$guard_release"
    guard_pid=$TEST_GUARD_PID
    wait_for_test_file "$guard_ready"
    launch_lock_contender "$lock_file" "$first_result" "$owner_release"
    first_pid=$LAST_LOCK_CONTENDER_PID
    launch_lock_contender "$lock_file" "$second_result" "$owner_release"
    second_pid=$LAST_LOCK_CONTENDER_PID
    wait_for_test_file "${first_result}.ready"
    wait_for_test_file "${second_result}.ready"
    if [ "$case_name" = atomic ] && [ -e "$lock_record" ]; then
        fail_test 'lock owner record appeared before the publication guard opened'
    elif [ "$case_name" = stale ] \
      && ! grep -Fq '"token": "stale.token"' "$lock_record"; then
        fail_test 'stale lock record changed while its replacement guard was held'
    fi
    : > "$guard_release"
    wait "$guard_pid"
    wait_for_test_file "$first_result"
    wait_for_test_file "$second_result"
    for result_file in "$first_result" "$second_result"; do
        case "$(<"$result_file")" in
            won:*) winners=$((winners + 1)) ;;
            lost) ;;
            *) fail_test "invalid $case_name lock contender result" ;;
        esac
    done
    [ "$winners" -eq 1 ] \
        || fail_test "$case_name lock race admitted $winners owners"
    python3 - "$lock_record" <<'PY'
import json
import os
import stat
import sys

path = sys.argv[1]
entry = os.lstat(path)
assert stat.S_ISREG(entry.st_mode)
with open(path, "r", encoding="utf-8") as handle:
    record = json.load(handle)
assert set(record) == {"format", "identity", "pid", "token"}
assert record["format"] == 1 and record["pid"] > 0 and record["token"]
PY
    : > "$owner_release"
    wait "$first_pid"
    wait "$second_pid"
    [ ! -e "$lock_record" ] \
        || fail_test "$case_name lock winner did not release its exact record"
}

ATOMIC_LOCK_FILE="$TMP_ROOT/atomic-lock/runtime/deploy-release.lock"
run_competing_lock_case atomic "$ATOMIC_LOCK_FILE"

STALE_LOCK_FILE="$TMP_ROOT/stale-lock/runtime/deploy-release.lock"
mkdir -p "$(dirname "$STALE_LOCK_FILE")"
python3 - "${STALE_LOCK_FILE}.d" <<'PY'
import json
import os
import sys
import tempfile

path = sys.argv[1]
record = {"format": 1, "identity": "dead-owner", "pid": 2147483647, "token": "stale.token"}
fd, temporary = tempfile.mkstemp(prefix=os.path.basename(path) + ".seed.", dir=os.path.dirname(path))
with os.fdopen(fd, "w", encoding="utf-8") as handle:
    json.dump(record, handle, sort_keys=True)
    handle.write("\n")
os.replace(temporary, path)
PY
run_competing_lock_case stale "$STALE_LOCK_FILE"

# A former owner must neither validate nor unlink a record atomically replaced
# under the guard. This is the ABA boundary cleanup paths rely on.
REPLACED_LOCK_FILE="$TMP_ROOT/replaced-lock/runtime/deploy-release.lock"
REPLACED_RECORD="${REPLACED_LOCK_FILE}.d"
REPLACED_READY="$TMP_ROOT/replaced-lock/owner.ready"
REPLACED_CHECK="$TMP_ROOT/replaced-lock/owner.check"
REPLACED_RESULT="$TMP_ROOT/replaced-lock/owner.result"
mkdir -p "$(dirname "$REPLACED_LOCK_FILE")"
bash -c '
    set -u
    . "$1"
    adk_acquire_routine_asset_lock "$2" 0
    : > "$3"
    while [ ! -e "$4" ]; do sleep 0.01; done
    if adk_routine_asset_lock_owned "$2"; then
        owned=owned
    else
        owned=replaced
    fi
    if adk_release_routine_asset_lock >/dev/null 2>&1; then
        released=released
    else
        released=refused
    fi
    printf "%s:%s\n" "$owned" "$released" > "$5"
' _ "$REPO_ROOT/scripts/routine-asset-surface.sh" "$REPLACED_LOCK_FILE" \
    "$REPLACED_READY" "$REPLACED_CHECK" "$REPLACED_RESULT" &
REPLACED_OWNER_PID=$!
wait_for_test_file "$REPLACED_READY"
REPLACEMENT_TOKEN='replacement.owner.token'
python3 - "$REPLACED_RECORD" "$$" "$REPLACEMENT_TOKEN" <<'PY'
import fcntl
import json
import os
import subprocess
import sys
import tempfile

path, pid_text, token = sys.argv[1:]
guard = os.open(path + ".guard", os.O_RDWR | os.O_CREAT, 0o600)
try:
    fcntl.flock(guard, fcntl.LOCK_EX)
    identity = subprocess.run(
        ["ps", "-o", "lstart=", "-p", pid_text],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    ).stdout.strip()
    record = {"format": 1, "identity": identity, "pid": int(pid_text), "token": token}
    fd, temporary = tempfile.mkstemp(prefix=os.path.basename(path) + ".replace.", dir=os.path.dirname(path))
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(record, handle, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
finally:
    os.close(guard)
PY
: > "$REPLACED_CHECK"
wait "$REPLACED_OWNER_PID"
if [ "$(<"$REPLACED_RESULT")" != 'replaced:refused' ] \
  || ! grep -Fq "\"token\": \"$REPLACEMENT_TOKEN\"" "$REPLACED_RECORD"; then
    fail_test 'replaced lock record was accepted or removed by its former owner'
fi
ADK_ROUTINE_ASSET_LOCK_DIR="$REPLACED_RECORD" \
    ADK_ROUTINE_ASSET_LOCK_TOKEN="$REPLACEMENT_TOKEN" \
    adk_release_routine_asset_lock

# A release migration marker binds this transaction to a compatible binary.
# Generic deploy/install recovery has no authority to roll it backward, even
# when the marker itself is corrupt or a broken symlink.
FORWARD_RUNTIME="$TMP_ROOT/forward-boundary-release"
FORWARD_SOURCE="$TMP_ROOT/forward-boundary-source"
seed_live "$FORWARD_RUNTIME" 'v0'
seed_source "$FORWARD_SOURCE" 'v1'
acquire_runtime_lock "$FORWARD_RUNTIME"
begin_staged_transaction "$FORWARD_SOURCE" "$FORWARD_RUNTIME"
FORWARD_TXN="$CURRENT_TXN"
FORWARD_MARKER="$FORWARD_TXN/forward-migration-applied.json"
printf 'invalid-but-durable-boundary\n' > "$FORWARD_MARKER"
if adk_recover_active_routine_asset_transaction "$FORWARD_RUNTIME" \
    >/dev/null 2>&1; then
    fail_test 'generic recovery accepted a forward-migration transaction'
fi
if adk_begin_routine_asset_transaction "$FORWARD_RUNTIME" "$CURRENT_LOCK" \
    >/dev/null 2>&1; then
    fail_test 'generic begin replaced a forward-migration transaction'
fi
[ -d "$FORWARD_TXN/staged/release-root/routines" ] \
    && [ -f "$FORWARD_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'generic recovery damaged the forward-migration transaction'
rm -f "$FORWARD_MARKER"
ln -s "$FORWARD_TXN/missing-forward-marker" "$FORWARD_MARKER"
if adk_recover_active_routine_asset_transaction "$FORWARD_RUNTIME" \
    >/dev/null 2>&1; then
    fail_test 'generic recovery ignored a broken forward-migration marker'
fi
rm -f "$FORWARD_MARKER"
adk_recover_active_routine_asset_transaction "$FORWARD_RUNTIME"
release_runtime_lock

# Invocation 1 can unload a candidate yet fail to prove its exact PID/port
# drained. Its fsynced authority marker must make invocation 2 fail closed;
# neither generic recovery nor begin may change one byte of the promoted pair.
DRAIN_RUNTIME="$TMP_ROOT/candidate-drain-release"
DRAIN_SOURCE="$TMP_ROOT/candidate-drain-source"
seed_live "$DRAIN_RUNTIME" 'v0'
seed_source "$DRAIN_SOURCE" 'v1'
acquire_runtime_lock "$DRAIN_RUNTIME"
begin_staged_transaction "$DRAIN_SOURCE" "$DRAIN_RUNTIME"
DRAIN_TXN="$CURRENT_TXN"
printf '%s\n' \
    '#!/bin/bash' \
    'set -euo pipefail' \
    '[ "${1:-}" = validate-routines ]' \
    '[ "${2:-}" = --root ]' \
    'root="${3:-}"' \
    '[ "${4:-}" = --runtime-root ]' \
    'runtime_root="${5:-}"' \
    '[ "$root" = "$runtime_root/routines" ]' \
    '[ -d "$root" ] && [ -d "$runtime_root/routine-helpers" ]' \
    > "$TMP_ROOT/candidate-binary"
chmod +x "$TMP_ROOT/candidate-binary"
adk_promote_routine_asset_transaction "$DRAIN_RUNTIME" "$DRAIN_TXN" \
    "$TMP_ROOT/candidate-binary"
adk_persist_routine_asset_candidate_drain_authority \
    "$DRAIN_RUNTIME" "$DRAIN_TXN" deploy 4242 'candidate-start-identity' \
    8791 'gui/test/com.agentdesk.release'
DRAIN_MARKER="$DRAIN_TXN/candidate-drain-required.json"
python3 - "$DRAIN_MARKER" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
expected = {
    "capture_state": "exact",
    "entrypoint": "deploy",
    "identity": "candidate-start-identity",
    "pid": "4242",
    "port": 8791,
    "supervisor": "gui/test/com.agentdesk.release",
}
raise SystemExit(0 if all(data.get(key) == value for key, value in expected.items()) else 1)
PY
if adk_recover_active_routine_asset_transaction "$DRAIN_RUNTIME" \
    >/dev/null 2>&1; then
    fail_test 'generic recovery ignored unresolved candidate drain authority'
fi
if adk_begin_routine_asset_transaction "$DRAIN_RUNTIME" "$CURRENT_LOCK" \
    >/dev/null 2>&1; then
    fail_test 'second invocation replaced unresolved candidate drain authority'
fi
[ "$(<"$DRAIN_RUNTIME/routines/generation-marker")" = v1 ] \
    && [ "$(<"$DRAIN_RUNTIME/routines.old/generation-marker")" = v0 ] \
    && [ -f "$DRAIN_RUNTIME/runtime/routine-assets.active" ] \
    && [ -f "$DRAIN_MARKER" ] \
    || fail_test 'second invocation mutated the unresolved candidate generation'
rm -f "$DRAIN_MARKER"
ln -s "$DRAIN_TXN/missing-candidate-drain" "$DRAIN_MARKER"
if adk_recover_active_routine_asset_transaction "$DRAIN_RUNTIME" \
    >/dev/null 2>&1; then
    fail_test 'generic recovery ignored a broken candidate drain authority'
fi
rm -f "$DRAIN_MARKER"
adk_rollback_routine_asset_transaction "$DRAIN_RUNTIME" "$DRAIN_TXN"
release_runtime_lock

# Once health has durably selected commit, retry/recovery must retain the new
# generation instead of rolling assets back beside the proven new binary.
INTENT_RUNTIME="$TMP_ROOT/commit-intent-release"
INTENT_SOURCE="$TMP_ROOT/commit-intent-source"
seed_live "$INTENT_RUNTIME" 'v0'
seed_source "$INTENT_SOURCE" 'v1'
acquire_runtime_lock "$INTENT_RUNTIME"
begin_staged_transaction "$INTENT_SOURCE" "$INTENT_RUNTIME"
INTENT_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$INTENT_RUNTIME" "$INTENT_TXN"
adk_mark_routine_asset_transaction_committing "$INTENT_RUNTIME" "$INTENT_TXN"
adk_recover_active_routine_asset_transaction "$INTENT_RUNTIME"
[ "$(<"$INTENT_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$INTENT_RUNTIME/routine-helpers/generation-marker")" = 'v1' ] \
    && [ ! -e "$INTENT_RUNTIME/routines.old" ] \
    && [ ! -e "$INTENT_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'durable commit intent recovered by rolling back the new generation'
release_runtime_lock

# v1 -> v2 commit cleanup failure leaves a durable `committing` transaction.
# The next lock holder must finish that commit, then v3 rollback must restore v2
# (the immediately previous live), never stale v1.
GEN_RUNTIME="$TMP_ROOT/generation-release"
GEN_V2_SOURCE="$TMP_ROOT/generation-v2"
GEN_V3_SOURCE="$TMP_ROOT/generation-v3"
seed_live "$GEN_RUNTIME" 'v1'
seed_source "$GEN_V2_SOURCE" 'v2'
seed_source "$GEN_V3_SOURCE" 'v3'
acquire_runtime_lock "$GEN_RUNTIME"
begin_staged_transaction "$GEN_V2_SOURCE" "$GEN_RUNTIME"
V2_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$GEN_RUNTIME" "$V2_TXN"

COMMIT_FAIL_PATH="$GEN_RUNTIME/routine-helpers.old"
rm() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$COMMIT_FAIL_PATH" ] || return 76
    done
    command rm "$@"
}
set +e
adk_commit_routine_asset_transaction "$GEN_RUNTIME" "$V2_TXN"
COMMIT_FAIL_STATUS=$?
set -e
unset -f rm
[ "$COMMIT_FAIL_STATUS" -ne 0 ] \
    && [ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routines.old/generation-marker")" = 'v1' ] \
    && [ "$(<"$V2_TXN/phase")" = 'committing' ] \
    || fail_test 'cleanup failure did not retain live v2 plus durable v1 cleanup state'
release_runtime_lock

# First recovery attempt fails the same cleanup again and must not manufacture
# or promote a new stage. The retry succeeds and bases v3 on authoritative v2.
acquire_runtime_lock "$GEN_RUNTIME"
rm() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$COMMIT_FAIL_PATH" ] || return 77
    done
    command rm "$@"
}
set +e
FAILED_BEGIN="$(
    adk_begin_routine_asset_transaction "$GEN_RUNTIME" "$CURRENT_LOCK"
)"
FAILED_BEGIN_STATUS=$?
set -e
unset -f rm
[ "$FAILED_BEGIN_STATUS" -ne 0 ] && [ -z "$FAILED_BEGIN" ] \
    && [ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    || fail_test 'failed committing recovery changed authoritative live v2'

begin_staged_transaction "$GEN_V3_SOURCE" "$GEN_RUNTIME"
V3_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
[ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v3' ] \
    && [ "$(<"$GEN_RUNTIME/routines.old/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routine-helpers.old/generation-marker")" = 'v2' ] \
    || fail_test 'v3 transaction used stale v1 instead of immediate live v2'
adk_rollback_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
adk_rollback_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
[ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routine-helpers/generation-marker")" = 'v2' ] \
    && [ ! -e "$GEN_RUNTIME/routines.old" ] \
    || fail_test 'v3 health rollback or double rollback failed to preserve v2'
release_runtime_lock

# TERM after live->old but before mv returns is recoverable because `armed` was
# durable before the first rename. The mv shim performs the rename, then returns
# a signal-like failure to force the exact interruption window.
SIGNAL_RUNTIME="$TMP_ROOT/signal-release"
SIGNAL_SOURCE="$TMP_ROOT/signal-source"
seed_live "$SIGNAL_RUNTIME" 'v0'
seed_source "$SIGNAL_SOURCE" 'v1'
acquire_runtime_lock "$SIGNAL_RUNTIME"
begin_staged_transaction "$SIGNAL_SOURCE" "$SIGNAL_RUNTIME"
SIGNAL_TXN="$CURRENT_TXN"
SIGNAL_FROM="$SIGNAL_RUNTIME/routines"
SIGNAL_TO="$SIGNAL_RUNTIME/routines.old"
SAW_DURABLE_ARM=0
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$SIGNAL_FROM" ] && [ "$2" = "$SIGNAL_TO" ]; then
        [ "$(<"$SIGNAL_TXN/phase")" = 'armed' ] && SAW_DURABLE_ARM=1
        command mv "$@"
        return 143
    fi
    command mv "$@"
}
set +e
adk_promote_routine_asset_transaction "$SIGNAL_RUNTIME" "$SIGNAL_TXN"
SIGNAL_STATUS=$?
set -e
unset -f mv
[ "$SIGNAL_STATUS" -ne 0 ] && [ "$SAW_DURABLE_ARM" = 1 ] \
    && [ ! -e "$SIGNAL_RUNTIME/routines" ] \
    && [ "$(<"$SIGNAL_RUNTIME/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$SIGNAL_TXN/phase")" = 'armed' ] \
    && [ -e "$SIGNAL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'signal-window state was not durably recoverable'
adk_rollback_routine_asset_transaction "$SIGNAL_RUNTIME" "$SIGNAL_TXN"
[ "$(<"$SIGNAL_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$SIGNAL_RUNTIME/routines.old" ] \
    && [ ! -e "$SIGNAL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'signal-window rollback did not recover v0'
release_runtime_lock

# An interrupted rollback preserves live/new in swap-current while old/v0 is
# still available. A second call converges without deleting either sole copy.
RETRY_RUNTIME="$TMP_ROOT/retry-release"
RETRY_SOURCE="$TMP_ROOT/retry-source"
seed_live "$RETRY_RUNTIME" 'v0'
seed_source "$RETRY_SOURCE" 'v1'
acquire_runtime_lock "$RETRY_RUNTIME"
begin_staged_transaction "$RETRY_SOURCE" "$RETRY_RUNTIME"
RETRY_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
ROLLBACK_FAIL_FROM="$RETRY_RUNTIME/routines.old"
ROLLBACK_FAIL_TO="$RETRY_RUNTIME/routines"
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$ROLLBACK_FAIL_FROM" ] \
      && [ "$2" = "$ROLLBACK_FAIL_TO" ]; then
        return 78
    fi
    command mv "$@"
}
set +e
adk_rollback_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
RETRY_FAIL_STATUS=$?
set -e
unset -f mv
[ "$RETRY_FAIL_STATUS" -ne 0 ] \
    && [ "$(<"$RETRY_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$RETRY_RUNTIME/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$RETRY_TXN/phase")" = 'rolling-back' ] \
    || fail_test 'failed rollback did not preserve both v0 and v1 copies'
adk_rollback_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
[ "$(<"$RETRY_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$RETRY_RUNTIME/routines.old" ] \
    && [ ! -e "$RETRY_RUNTIME/routines.swap-current" ] \
    || fail_test 'rollback retry failed to converge to v0'
release_runtime_lock

# Fresh surfaces roll back to absence, and a second rollback is a no-op.
FRESH_RUNTIME="$TMP_ROOT/fresh-release"
FRESH_SOURCE="$TMP_ROOT/fresh-source"
seed_source "$FRESH_SOURCE" 'v1'
acquire_runtime_lock "$FRESH_RUNTIME"
begin_staged_transaction "$FRESH_SOURCE" "$FRESH_RUNTIME"
FRESH_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
adk_rollback_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
adk_rollback_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
[ ! -e "$FRESH_RUNTIME/routines" ] && [ ! -e "$FRESH_RUNTIME/routine-helpers" ] \
    || fail_test 'fresh transaction rollback did not restore absent surfaces'
release_runtime_lock

# Pre-state-machine interruption may leave the only copies under .old. Beginning
# a new transaction restores both sole copies before it creates a unique stage.
SOLE_RUNTIME="$TMP_ROOT/sole-old-release"
seed_live "$SOLE_RUNTIME" 'v0'
mv "$SOLE_RUNTIME/routines" "$SOLE_RUNTIME/routines.old"
mv "$SOLE_RUNTIME/routine-helpers" "$SOLE_RUNTIME/routine-helpers.old"
acquire_runtime_lock "$SOLE_RUNTIME"
SOLE_TXN="$(adk_begin_routine_asset_transaction "$SOLE_RUNTIME" "$CURRENT_LOCK")"
[ "$(<"$SOLE_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$SOLE_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    && [ ! -e "$SOLE_RUNTIME/routines.old" ] \
    && [ ! -e "$SOLE_RUNTIME/routine-helpers.old" ] \
    || fail_test 'sole .old copies were not restored before a new transaction'
adk_abort_routine_asset_transaction "$SOLE_RUNTIME" "$SOLE_TXN"
release_runtime_lock

# The shared lock rejects a second entrypoint, and an old transaction cannot
# promote a later transaction's unique stage.
LOCK_RUNTIME="$TMP_ROOT/lock-release"
LOCK_SOURCE="$TMP_ROOT/lock-source"
seed_source "$LOCK_SOURCE" 'v1'
acquire_runtime_lock "$LOCK_RUNTIME"
begin_staged_transaction "$LOCK_SOURCE" "$LOCK_RUNTIME"
LOCK_TXN="$CURRENT_TXN"
set +e
(
    ADK_ROUTINE_ASSET_LOCK_DIR="" \
        ADK_ROUTINE_ASSET_LOCK_TOKEN="" \
        adk_acquire_routine_asset_lock "$CURRENT_LOCK" 0
) >/dev/null 2>&1
SECOND_LOCK_STATUS=$?
set -e
[ "$SECOND_LOCK_STATUS" -ne 0 ] || fail_test 'second entrypoint acquired the shared lock'
adk_abort_routine_asset_transaction "$LOCK_RUNTIME" "$LOCK_TXN"
LATER_TXN="$(adk_begin_routine_asset_transaction "$LOCK_RUNTIME" "$CURRENT_LOCK")"
set +e
adk_promote_routine_asset_transaction "$LOCK_RUNTIME" "$LOCK_TXN" >/dev/null 2>&1
OLD_PROMOTE_STATUS=$?
set -e
[ "$OLD_PROMOTE_STATUS" -ne 0 ] \
    || fail_test 'closed transaction promoted a later unique payload'
adk_abort_routine_asset_transaction "$LOCK_RUNTIME" "$LATER_TXN"
release_runtime_lock

# The bounded lexical validator ignores registration marker text in comments,
# strings, and templates, rejects an empty bundled root, and accepts a real
# standalone program-level call even when decoys are present.
assert_invalid_routine_source() {
    local name="$1"
    local body="$2"
    local root="$TMP_ROOT/lexer-$name"

    mkdir -p "$root/routines"
    seed_required_helpers "$root/routine-helpers" "$name"
    printf '%s\n' "$body" > "$root/routines/mutant.js"
    if adk_validate_quickjs_routine_tree "$root/routines" >/dev/null 2>&1; then
        fail_test "lexical validator accepted $name marker mutant"
    fi
}

assert_invalid_routine_source block-comment \
    '/* agentdesk.routines.register({ tick() { return { action: "complete" }; } }); */ module.exports = {};'
assert_invalid_routine_source string \
    'const marker = "agentdesk.routines.register({ tick() {} })"; module.exports = {};'
assert_invalid_routine_source template \
    'const marker = `agentdesk.routines.register({ tick() {} })`; module.exports = {};'
assert_invalid_routine_source nested \
    'function later() { agentdesk.routines.register({ tick() { return { action: "complete" }; } }); }'
assert_invalid_routine_source malformed-tail \
    'agentdesk.routines.register({ tick() { return { action: "complete" }; } }); function broken() {'
assert_invalid_routine_source shadowed-agentdesk \
    'const agentdesk = { routines: { register() {} } }; agentdesk.routines.register({ name: "fake", tick() { return { action: "complete" }; } });'
assert_invalid_routine_source empty-register \
    'agentdesk.routines.register();'
assert_invalid_routine_source missing-tick \
    'agentdesk.routines.register({ name: "missing" });'
assert_invalid_routine_source noncallable-tick \
    'agentdesk.routines.register({ name: "primitive", tick: true });'
EMPTY_ROOT="$TMP_ROOT/lexer-empty"
mkdir -p "$EMPTY_ROOT/routines"
seed_required_helpers "$EMPTY_ROOT/routine-helpers" 'empty'
if adk_validate_quickjs_routine_tree "$EMPTY_ROOT/routines" >/dev/null 2>&1; then
    fail_test 'empty bundled routines root passed validation'
fi
VALID_ROOT="$TMP_ROOT/lexer-valid"
seed_required_helpers "$VALID_ROOT/routine-helpers" 'valid'
mkdir -p "$VALID_ROOT/routines"
printf '%s\n' \
    '/* agentdesk.routines.register({}); */' \
    'agentdesk.routines.register({ name: "real", tick() { return { action: "complete" }; } });' \
    > "$VALID_ROOT/routines/valid.js"
printf '%s\n' \
    'agentdesk.routines.register({ name: "function", tick: function(ctx) { return { action: "complete" }; } });' \
    > "$VALID_ROOT/routines/function-value.js"
printf '%s\n' \
    'agentdesk.routines.register({ name: "arrow", tick: (ctx) => ({ action: "complete" }) });' \
    > "$VALID_ROOT/routines/arrow-value.js"
adk_validate_quickjs_routine_tree "$VALID_ROOT/routines"
printf '%s\n' 'const decoy = "agentdesk.routines.register({})";' \
    > "$VALID_ROOT/routines/second-invalid.js"
if adk_validate_quickjs_routine_tree "$VALID_ROOT/routines" >/dev/null 2>&1; then
    fail_test 'validator did not require a real registration in every JavaScript file'
fi
rm "$VALID_ROOT/routines/second-invalid.js"

# Node/Python helpers must not leak back into the QuickJS entrypoint root, and
# source/root symlinks remain fail-closed before rsync can follow them.
mkdir -p "$VALID_ROOT/routines/monitoring"
printf '%s\n' 'module.exports = {};' \
    > "$VALID_ROOT/routines/monitoring/local_worktree_inventory.js"
if adk_validate_quickjs_routine_tree "$VALID_ROOT/routines" >/dev/null 2>&1; then
    fail_test 'non-QuickJS helper copied into routines passed validation'
fi
rm "$VALID_ROOT/routines/monitoring/local_worktree_inventory.js"

# Lexical inspection is diagnostic only. Runtime-valid registrations can build
# their spec programmatically; exact candidate evaluation owns acceptance.
PROGRAMMATIC_ROOT="$TMP_ROOT/programmatic-registration"
seed_required_helpers "$PROGRAMMATIC_ROOT/routine-helpers" 'programmatic'
mkdir -p "$PROGRAMMATIC_ROOT/routines"
printf '%s\n' \
    'const spec = { name: "programmatic", tick() { return { action: "complete" }; } };' \
    'agentdesk.routines.register(spec);' \
    > "$PROGRAMMATIC_ROOT/routines/programmatic.js"
if adk_validate_quickjs_routine_tree \
    "$PROGRAMMATIC_ROOT/routines" >/dev/null 2>&1; then
    fail_test 'lexical diagnostic unexpectedly modeled programmatic registration'
fi
adk_validate_repo_routine_assets "$PROGRAMMATIC_ROOT" >/dev/null 2>&1 \
    || fail_test 'lexical diagnostic remained an authoritative production gate'

LINK_ROOT="$TMP_ROOT/symlink-source"
seed_source "$LINK_ROOT" 'linked'
mv "$LINK_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
    "$TMP_ROOT/linked-helper.py"
ln -s "$TMP_ROOT/linked-helper.py" \
    "$LINK_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
if adk_validate_repo_routine_assets "$LINK_ROOT" >/dev/null 2>&1; then
    fail_test 'routine helper descendant symlink passed validation'
fi
ln -s "$LINK_ROOT" "$TMP_ROOT/symlink-source-root"
if adk_validate_repo_routine_assets "$TMP_ROOT/symlink-source-root" >/dev/null 2>&1; then
    fail_test 'symlinked routine asset source root passed validation'
fi

# Missing required source fails before transaction start; rsync failure leaves
# live assets untouched and a staging transaction safely abortable.
MISSING_ROOT="$TMP_ROOT/missing-source"
seed_source "$MISSING_ROOT" 'bad'
rm "$MISSING_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
if adk_validate_repo_routine_assets "$MISSING_ROOT" >/dev/null 2>&1; then
    fail_test 'missing authoritative helper passed source preflight'
fi
RSYNC_SOURCE="$TMP_ROOT/rsync-source"
RSYNC_RUNTIME="$TMP_ROOT/rsync-release"
seed_source "$RSYNC_SOURCE" 'v1'
seed_live "$RSYNC_RUNTIME" 'v0'
acquire_runtime_lock "$RSYNC_RUNTIME"
RSYNC_TXN="$(adk_begin_routine_asset_transaction "$RSYNC_RUNTIME" "$CURRENT_LOCK")"
RSYNC_FAIL_SOURCE="$RSYNC_SOURCE/routine-helpers/"
rsync() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$RSYNC_FAIL_SOURCE" ] || return 73
    done
    command rsync "$@"
}
set +e
adk_stage_routine_helpers "$RSYNC_SOURCE" "$RSYNC_RUNTIME" "$RSYNC_TXN" >/dev/null
RSYNC_STATUS=$?
set -e
unset -f rsync
[ "$RSYNC_STATUS" -ne 0 ] \
    && [ "$(<"$RSYNC_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    && [ ! -e "$RSYNC_TXN/staged/release-root/routine-helpers" ] \
    || fail_test 'rsync failure was masked or changed live helpers'
adk_abort_routine_asset_transaction "$RSYNC_RUNTIME" "$RSYNC_TXN"
release_runtime_lock

# Peer transport uses a validated absolute root, quoted fake-ssh command, and
# rsync --protect-args so spaces and shell metacharacters stay one remote path.
PEER_ROOT="$TMP_ROOT/peer/ADK Root;[\$HOME]"
mkdir -p "$PEER_ROOT/routines" "$PEER_ROOT/routine-helpers"
PEER_SSH_ARGS=()
ssh() {
    PEER_SSH_ARGS=("$@")
    command bash -c "${4:?missing fake-ssh remote command}"
}
adk_guard_peer_routine_asset_paths 'operator@peer.local' "$PEER_ROOT" 7
[ "${#PEER_SSH_ARGS[@]}" -ge 4 ] \
    && [[ "${PEER_SSH_ARGS[*]}" == *'bash -lc '* ]] \
    || fail_test 'fake ssh did not receive the quoted peer guard command'
ln -s "$TMP_ROOT" "$PEER_ROOT/routines/operator-link"
if adk_guard_peer_routine_asset_paths 'operator@peer.local' "$PEER_ROOT" 7 \
  >/dev/null 2>&1; then
    fail_test 'quoted peer guard missed a symlink under a metacharacter path'
fi
rm "$PEER_ROOT/routines/operator-link"
unset -f ssh
if adk_guard_peer_routine_asset_paths '-oProxyCommand=evil' "$PEER_ROOT" 7 \
  >/dev/null 2>&1; then
    fail_test 'peer guard accepted an option-like SSH destination'
fi
PEER_RSYNC_ARGS=()
PEER_FAST_PROBE=0
ssh() {
    case "${4:-}" in
        *rsync*--protect-args*--version*)
            PEER_FAST_PROBE=$((PEER_FAST_PROBE + 1))
            return 0
            ;;
        *) return 93 ;;
    esac
}
rsync() {
    PEER_RSYNC_ARGS=("$@")
    return 0
}
adk_rsync_peer_asset_surface "$VALID_ROOT/routines" 'operator@peer.local' \
    "$PEER_ROOT" 'routines' 7
unset -f rsync ssh
[ "$PEER_FAST_PROBE" -eq 1 ] \
    || fail_test 'peer rsync fast path did not handshake remote protect-args support'
printf '%s\n' "${PEER_RSYNC_ARGS[@]}" | grep -Fxq -- '--protect-args' \
    || fail_test 'peer rsync omitted --protect-args'
[ "${PEER_RSYNC_ARGS[${#PEER_RSYNC_ARGS[@]}-1]}" = \
    "operator@peer.local:$PEER_ROOT/routines/" ] \
    || fail_test 'peer rsync split or rewrote the remote metacharacter path'

# A modern local rsync is insufficient when the remote stock macOS rsync
# rejects protect-args. The capability matrix must choose the quoted tar path
# without ever attempting an unsafe/unprotected rsync transfer.
REMOTE_LEGACY_ROOT="$TMP_ROOT/remote legacy/ADK Root;[\$HOME]"
REMOTE_LEGACY_PROBE_MARKER="$TMP_ROOT/remote-legacy.probe"
REMOTE_LEGACY_TAR_MARKER="$TMP_ROOT/remote-legacy.tar"
REMOTE_LEGACY_LOCAL_PROBES=0
REMOTE_LEGACY_RSYNC_TRANSFERS=0
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        REMOTE_LEGACY_LOCAL_PROBES=$((REMOTE_LEGACY_LOCAL_PROBES + 1))
        return 0
    fi
    REMOTE_LEGACY_RSYNC_TRANSFERS=$((REMOTE_LEGACY_RSYNC_TRANSFERS + 1))
    return 94
}
ssh() {
    case "${4:-}" in
        *rsync*--protect-args*--version*)
            : > "$REMOTE_LEGACY_PROBE_MARKER"
            return 1
            ;;
        *)
            : > "$REMOTE_LEGACY_TAR_MARKER"
            command bash -c "${4:?missing remote-legacy fake-ssh command}"
            ;;
    esac
}
adk_rsync_peer_asset_surface "$VALID_ROOT/routines" 'operator@peer.local' \
    "$REMOTE_LEGACY_ROOT" 'routines' 7
unset -f ssh rsync
if [ "$REMOTE_LEGACY_LOCAL_PROBES" -ne 1 ] \
  || [ "$REMOTE_LEGACY_RSYNC_TRANSFERS" -ne 0 ] \
  || [ ! -f "$REMOTE_LEGACY_PROBE_MARKER" ] \
  || [ ! -f "$REMOTE_LEGACY_TAR_MARKER" ] \
  || ! cmp "$VALID_ROOT/routines/valid.js" \
      "$REMOTE_LEGACY_ROOT/routines/valid.js" >/dev/null; then
    fail_test 'local-modern remote-legacy matrix bypassed safe tar fallback'
fi

# Legacy macOS rsync rejects --protect-args. Its fallback must transfer the
# same metacharacter path through a quoted tar-over-SSH command, never retry an
# unprotected rsync invocation.
LEGACY_ROOT="$TMP_ROOT/legacy peer/ADK Root;[\$HOME]"
mkdir -p "$(dirname "$LEGACY_ROOT")"
LEGACY_RSYNC_PROBES=0
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        LEGACY_RSYNC_PROBES=$((LEGACY_RSYNC_PROBES + 1))
        return 1
    fi
    return 91
}
ssh() {
    command bash -c "${4:?missing legacy fake-ssh remote command}"
}
adk_rsync_peer_asset_surface "$VALID_ROOT/routines" 'operator@peer.local' \
    "$LEGACY_ROOT" 'routines' 7
unset -f ssh rsync
[ "$LEGACY_RSYNC_PROBES" -eq 1 ] \
    && cmp "$VALID_ROOT/routines/valid.js" \
        "$LEGACY_ROOT/routines/valid.js" >/dev/null \
    || fail_test 'legacy rsync fallback lost quoted tar-stream assets'

# Peer pre-sync owns only a unique inbox. The receiving deploy must hold its
# shared lock to claim it; staging merges operator files while repository bytes
# remain authoritative, and live v0 stays untouched until explicit promotion.
INBOX_RUNTIME="$TMP_ROOT/peer incoming runtime"
INBOX_SOURCE="$TMP_ROOT/peer-incoming-source"
INBOX_REPO="$TMP_ROOT/peer-incoming-repo"
seed_live "$INBOX_RUNTIME" 'v0'
seed_source "$INBOX_SOURCE" 'v1'
seed_source "$INBOX_REPO" 'v2'
write_quickjs_routine "$INBOX_SOURCE/routines/operator-private.js" 'operator'
printf 'operator helper\n' > "$INBOX_SOURCE/routine-helpers/operator-private.py"
INBOX_TOKEN='test.123.safe'
ssh() {
    command bash -c "${4:?missing inbox fake-ssh remote command}"
}
INBOX_PATH="$(adk_prepare_peer_asset_incoming 'operator@peer.local' \
    "$INBOX_RUNTIME" "$INBOX_TOKEN" 7)"
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        return 1
    fi
    return 91
}
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routines" 'operator@peer.local' \
    "$INBOX_PATH" 'routines' 7
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routine-helpers" 'operator@peer.local' \
    "$INBOX_PATH" 'routine-helpers' 7
unset -f rsync ssh
[ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$INBOX_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    || fail_test 'peer inbox transfer mutated live surfaces before remote lock'
if adk_claim_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" \
    "$INBOX_RUNTIME/runtime/deploy-release.lock" >/dev/null 2>&1; then
    fail_test 'peer inbox claim succeeded without owning the shared lock'
fi

# Sender disconnect cleanup must take the receiver's exact custom deploy guard:
# <lock>.d.guard. Hold that guard across receiver owner-record + claim
# publication and prove cleanup blocks, then refuses deletion after publication.
RACE_TOKEN='guard-race.123.safe'
ssh() {
    command bash -c "${4:?missing guard-race prepare command}"
}
RACE_PATH="$(adk_prepare_peer_asset_incoming 'operator@peer.local' \
    "$INBOX_RUNTIME" "$RACE_TOKEN" 7)"
unset -f ssh
RACE_LOCK="$INBOX_RUNTIME/runtime/custom-receiver.lock"
RACE_READY="$TMP_ROOT/peer-guard.ready"
RACE_PUBLISH="$TMP_ROOT/peer-guard.publish"
RACE_CLEANUP_ENTERED="$TMP_ROOT/peer-cleanup.entered"
RACE_CLEANUP_RESULT="$TMP_ROOT/peer-cleanup.result"
hold_test_peer_claim_guard "$RACE_LOCK" "$RACE_PATH" \
    "$RACE_READY" "$RACE_PUBLISH"
wait_for_test_file "$RACE_READY"
ssh() {
    : > "$RACE_CLEANUP_ENTERED"
    command bash -c "${4:?missing guard-race cleanup command}"
}
(
    if adk_remove_peer_asset_incoming 'operator@peer.local' "$INBOX_RUNTIME" \
        "$RACE_TOKEN" 7 "$RACE_LOCK" >/dev/null 2>&1; then
        printf 'deleted\n' > "$RACE_CLEANUP_RESULT"
    else
        printf 'refused\n' > "$RACE_CLEANUP_RESULT"
    fi
) &
RACE_CLEANUP_PID=$!
wait_for_test_file "$RACE_CLEANUP_ENTERED"
kill -0 "$RACE_CLEANUP_PID" 2>/dev/null \
    && [ ! -e "$RACE_CLEANUP_RESULT" ] \
    && [ -d "$RACE_PATH" ] \
    || fail_test 'sender cleanup did not block on receiver custom .d.guard'
: > "$RACE_PUBLISH"
wait "$TEST_GUARD_PID"
wait "$RACE_CLEANUP_PID"
unset -f ssh
[ "$(<"$RACE_CLEANUP_RESULT")" = refused ] \
    && [ -f "$RACE_LOCK.d" ] \
    && [ -f "$RACE_PATH/.claimed" ] \
    || fail_test 'sender cleanup deleted inbox after receiver lock/claim publication'
rm -rf "$RACE_PATH"
rm -f "$RACE_LOCK.d" "$RACE_LOCK.d.guard"

CURRENT_LOCK="$INBOX_RUNTIME/runtime/custom-deploy.lock"
adk_acquire_routine_asset_lock "$CURRENT_LOCK" 0
ssh() {
    command bash -c "${4:?missing claimed-inbox fake-ssh command}"
}
# Receiver claim is a two-step protocol: lock publication, then .claimed
# publication. Sender cleanup must refuse the inbox throughout that exact gap,
# not only after the marker appears.
if adk_remove_peer_asset_incoming 'operator@peer.local' "$INBOX_RUNTIME" \
    "$INBOX_TOKEN" 7 "$CURRENT_LOCK" >/dev/null 2>&1; then
    fail_test 'sender cleanup won the receiver lock-to-claim race'
fi
[ -d "$INBOX_PATH" ] && [ ! -e "$INBOX_PATH/.claimed" ] \
    || fail_test 'sender cleanup damaged the unclaimed inbox of the remote lock owner'
adk_claim_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" "$CURRENT_LOCK"
if adk_remove_peer_asset_incoming 'operator@peer.local' "$INBOX_RUNTIME" \
    "$INBOX_TOKEN" 7 "$CURRENT_LOCK" >/dev/null 2>&1; then
    fail_test 'sender cleanup removed an inbox already claimed by the remote lock owner'
fi
unset -f ssh
[ -d "$INBOX_PATH" ] && [ -f "$INBOX_PATH/.claimed" ] \
    || fail_test 'failed sender cleanup damaged the remote-owned claimed inbox'
INBOX_TXN="$(adk_begin_routine_asset_transaction "$INBOX_RUNTIME" "$CURRENT_LOCK")"
adk_stage_routines "$INBOX_REPO" "$INBOX_RUNTIME" "$INBOX_TXN" \
    "$INBOX_PATH/routines" >/dev/null
adk_stage_routine_helpers "$INBOX_REPO" "$INBOX_RUNTIME" "$INBOX_TXN" \
    "$INBOX_PATH/routine-helpers" >/dev/null
[ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$INBOX_TXN/staged/release-root/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$INBOX_TXN/staged/release-root/routine-helpers/generation-marker")" = 'v2' ] \
    && [ -f "$INBOX_TXN/staged/release-root/routines/operator-private.js" ] \
    && [ -f "$INBOX_TXN/staged/release-root/routine-helpers/operator-private.py" ] \
    || fail_test 'remote lock-owned inbox staging changed live or lost overlay precedence'
adk_abort_routine_asset_transaction "$INBOX_RUNTIME" "$INBOX_TXN"
adk_remove_claimed_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" "$CURRENT_LOCK"
release_runtime_lock
[ ! -e "$INBOX_PATH" ] \
    && [ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    || fail_test 'aborted peer inbox transaction changed live or leaked its inbox'

# A partial transfer is inert and removable: without both surfaces the remote
# preflight cannot consume it, while the old live pair remains exact.
PARTIAL_TOKEN='partial.456.safe'
ssh() {
    command bash -c "${4:?missing partial-inbox fake-ssh command}"
}
PARTIAL_PATH="$(adk_prepare_peer_asset_incoming 'operator@peer.local' \
    "$INBOX_RUNTIME" "$PARTIAL_TOKEN" 7)"
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        return 1
    fi
    return 91
}
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routines" 'operator@peer.local' \
    "$PARTIAL_PATH" 'routines' 7
unset -f rsync
[ -d "$PARTIAL_PATH/routines" ] \
    && [ ! -e "$PARTIAL_PATH/routine-helpers" ] \
    && [ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    || fail_test 'partial peer transfer escaped its isolated inbox'
adk_remove_peer_asset_incoming 'operator@peer.local' "$INBOX_RUNTIME" \
    "$PARTIAL_TOKEN" 7
unset -f ssh
[ ! -e "$PARTIAL_PATH" ] \
    || fail_test 'partial peer transfer inbox could not be safely removed'

# Exercise installer preflight and promotion functions. An incomplete old
# artifact must fail before a binary marker or live assets change.
extract_function() {
    local function_name="$1"
    local source_file="${2:-$REPO_ROOT/scripts/install.sh}"
    awk -v start="^${function_name}[(][)] [{]$" '
        printing && $0 ~ /^[A-Za-z_][A-Za-z0-9_]*[(][)] [{]$/ { exit }
        $0 ~ start { printing = 1 }
        printing { print }
    ' "$source_file"
}
eval "$(extract_function prepare_install_routine_asset_surfaces)"
eval "$(extract_function promote_install_routine_asset_surfaces)"
eval "$(extract_function finalize_install_routine_asset_surfaces)"
eval "$(extract_function _install_sha256_file)"
eval "$(extract_function _install_binary_immutable_flag_state)"
eval "$(extract_function _install_binary_has_immutable_flag)"
eval "$(extract_function _install_apply_binary_immutable_flag_state)"
eval "$(extract_function _install_clear_binary_immutable_flag)"
eval "$(extract_function _install_restore_old_binary_flag)"
eval "$(extract_function sign_binary_with_fallback)"
eval "$(extract_function prepare_install_binary_transaction)"
eval "$(extract_function promote_install_binary_transaction)"
eval "$(extract_function _install_binary_live_sha256)"
eval "$(extract_function _install_binary_is_promoted)"
eval "$(extract_function _restore_install_binary_transaction)"
eval "$(extract_function _install_cleanup)"
eval "$(extract_function _install_service_job_is_loaded)"
eval "$(extract_function _install_current_service_pid)"
eval "$(extract_function _capture_install_service_process)"
eval "$(extract_function _capture_install_candidate_process)"
eval "$(extract_function _persist_install_candidate_drain_authority)"
eval "$(extract_function capture_install_candidate_process_after_start)"
eval "$(extract_function _install_candidate_process_is_alive)"
eval "$(extract_function _install_candidate_port_refuses_connections)"
eval "$(extract_function _install_candidate_drain_is_proven)"
eval "$(extract_function wait_for_install_candidate_stop)"
eval "$(extract_function _force_stop_install_candidate)"
eval "$(extract_function install_service_is_running)"
eval "$(extract_function wait_for_install_service_stop)"
eval "$(extract_function stop_install_service_for_promotion)"
eval "$(extract_function start_install_service)"
eval "$(extract_function stop_install_service_for_recovery)"
eval "$(extract_function restart_previous_install_service)"
warn() { :; }

reset_install_transaction_state() {
    INSTALL_ROUTINE_ASSET_TXN=""
    INSTALL_ROUTINE_ASSET_RUNTIME=""
    INSTALL_BINARY_LIVE=""
    INSTALL_BINARY_STAGE=""
    INSTALL_BINARY_BACKUP=""
    INSTALL_BINARY_NEW_SHA256=""
    INSTALL_BINARY_OLD_SHA256=""
    INSTALL_BINARY_HAD_LIVE=0
    INSTALL_BINARY_OLD_IMMUTABLE=0
    INSTALL_BINARY_FLAG_SNAPSHOT_TAKEN=0
    INSTALL_BINARY_SWAP_ARMED=0
    INSTALL_BINARY_PROMOTED=0
    INSTALL_COMMIT_INTENT=0
    INSTALL_ASSET_FINALIZED=0
    INSTALL_LOCK_FILE=""
    INSTALL_LOCK_HELD=0
    INSTALL_SERVICE_WAS_RUNNING=0
    INSTALL_SERVICE_STOP_ATTEMPTED=0
    INSTALL_SERVICE_STOP_CONFIRMED=0
    INSTALL_SERVICE_START_ATTEMPTED=0
    INSTALL_SERVICE_START_CONFIRMED=0
    INSTALL_SERVICE_HEALTHY=0
    INSTALL_LAUNCHD_DOMAIN=""
    INSTALL_SERVICE_OLD_PID=""
    INSTALL_SERVICE_OLD_IDENTITY=""
    INSTALL_SERVICE_CANDIDATE_PID=""
    INSTALL_SERVICE_CANDIDATE_IDENTITY=""

    [ -z "$INSTALL_ROUTINE_ASSET_TXN" ] \
        && [ -z "$INSTALL_ROUTINE_ASSET_RUNTIME" ] \
        && [ -z "$INSTALL_BINARY_LIVE" ] \
        && [ -z "$INSTALL_BINARY_STAGE" ] \
        && [ -z "$INSTALL_BINARY_BACKUP" ] \
        && [ -z "$INSTALL_BINARY_NEW_SHA256" ] \
        && [ -z "$INSTALL_BINARY_OLD_SHA256" ] \
        && [ "$INSTALL_BINARY_HAD_LIVE" -eq 0 ] \
        && [ "$INSTALL_BINARY_FLAG_SNAPSHOT_TAKEN" -eq 0 ] \
        && [ "$INSTALL_BINARY_SWAP_ARMED" -eq 0 ] \
        && [ "$INSTALL_BINARY_PROMOTED" -eq 0 ] \
        && [ "$INSTALL_COMMIT_INTENT" -eq 0 ] \
        && [ "$INSTALL_ASSET_FINALIZED" -eq 0 ]
}

assert_install_generation() {
    local runtime_root="$1"
    local generation="$2"
    local binary="$3"

    [ "$(<"$runtime_root/routines/generation-marker")" = "$generation" ] \
        && [ "$(<"$runtime_root/routine-helpers/generation-marker")" = "$generation" ] \
        && [ "$(<"$runtime_root/bin/agentdesk")" = "$binary" ] \
        && [ ! -e "$runtime_root/runtime/routine-assets.active" ]
}

make_install_runtime() {
    local runtime_root="$1"
    seed_live "$runtime_root" 'v0'
    mkdir -p "$runtime_root/bin"
    printf 'old-binary\n' > "$runtime_root/bin/agentdesk"
    chmod +x "$runtime_root/bin/agentdesk"
}

write_fake_install_candidate() {
    local path="$1"

    printf '%s\n' \
        '#!/bin/bash' \
        'set -euo pipefail' \
        '[ "${1:-}" = validate-routines ]' \
        '[ "${2:-}" = --root ]' \
        'root="${3:-}"' \
        '[ "${4:-}" = --runtime-root ]' \
        'runtime_root="${5:-}"' \
        '[ "$root" = "$runtime_root/routines" ]' \
        '[ -d "$root" ] && [ -d "$runtime_root/routine-helpers" ]' \
        '# new-binary' \
        > "$path"
    chmod +x "$path"
}

BAD_ARTIFACT="$TMP_ROOT/old-artifact"
INSTALL_RUNTIME="$TMP_ROOT/install-release"
mkdir -p "$BAD_ARTIFACT/scripts" "$BAD_ARTIFACT/routines" \
    "$INSTALL_RUNTIME/bin"
cp "$REPO_ROOT/scripts/routine-asset-surface.sh" "$BAD_ARTIFACT/scripts/"
cp "$REPO_ROOT/scripts/validate-quickjs-routines.py" "$BAD_ARTIFACT/scripts/"
write_quickjs_routine "$BAD_ARTIFACT/routines/only.js" 'old-artifact'
seed_live "$INSTALL_RUNTIME" 'v0'
printf 'original binary\n' > "$INSTALL_RUNTIME/bin/agentdesk"
set +e
prepare_install_routine_asset_surfaces "$BAD_ARTIFACT" "$INSTALL_RUNTIME"
BAD_PREPARE_STATUS=$?
set -e
[ "$BAD_PREPARE_STATUS" -ne 0 ] \
    && [ "$(<"$INSTALL_RUNTIME/bin/agentdesk")" = 'original binary' ] \
    && [ "$(<"$INSTALL_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$INSTALL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'old artifact preflight mutated binary or assets'

GOOD_ARTIFACT="$TMP_ROOT/good-artifact"
seed_source "$GOOD_ARTIFACT" 'v1'
mkdir -p "$GOOD_ARTIFACT/scripts"
cp "$REPO_ROOT/scripts/routine-asset-surface.sh" "$GOOD_ARTIFACT/scripts/"
cp "$REPO_ROOT/scripts/validate-quickjs-routines.py" "$GOOD_ARTIFACT/scripts/"
write_fake_install_candidate "$GOOD_ARTIFACT/agentdesk"

# A failed signing operation cannot be laundered by verification of a signature
# already present on the copied payload. The sign status itself is authoritative.
SIGN_FAILURE_TARGET="$TMP_ROOT/sign-failure-candidate"
cp "$GOOD_ARTIFACT/agentdesk" "$SIGN_FAILURE_TARGET"
SIGN_FAILURE_EVENTS=''
CODESIGN_IDENTITY='-'
codesign() {
    case "${1:-}" in
        -s)
            SIGN_FAILURE_EVENTS="${SIGN_FAILURE_EVENTS}sign "
            return 42
            ;;
        -v)
            SIGN_FAILURE_EVENTS="${SIGN_FAILURE_EVENTS}verify "
            return 0
            ;;
    esac
}
set +e
sign_binary_with_fallback "$SIGN_FAILURE_TARGET" >/dev/null 2>&1
SIGN_FAILURE_STATUS=$?
set -e
unset -f codesign
[ "$SIGN_FAILURE_STATUS" -ne 0 ] && [ "$SIGN_FAILURE_EVENTS" = 'sign ' ] \
    || fail_test 'failed codesign operation was accepted through verification'

# A binary copy failure occurs while both live surfaces still carry v0.
COPY_FAIL_RUNTIME="$TMP_ROOT/install-copy-fail"
make_install_runtime "$COPY_FAIL_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$COPY_FAIL_RUNTIME"
    cp() {
        [ "${1:-}" != "$GOOD_ARTIFACT/agentdesk" ] || return 71
        command cp "$@"
    }
    set +e
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$COPY_FAIL_RUNTIME"
    copy_status=$?
    unset -f cp
    _install_cleanup "$copy_status"
    cleanup_status=$?
    set -e
    [ "$copy_status" -ne 0 ] && [ "$cleanup_status" -ne 0 ]
) || fail_test 'installer masked staged binary copy failure'
assert_install_generation "$COPY_FAIL_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'binary copy failure produced an old-binary/new-assets pair'

# Failure after asset promotion but before the binary rename rolls assets back.
PRE_RENAME_RUNTIME="$TMP_ROOT/install-pre-rename"
make_install_runtime "$PRE_RENAME_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$PRE_RENAME_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$PRE_RENAME_RUNTIME"
    promote_install_routine_asset_surfaces
    set +e
    _install_cleanup 72
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 72 ]
) || fail_test 'installer cleanup failed at the assets-to-binary boundary'
assert_install_generation "$PRE_RENAME_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'pre-rename failure left promoted assets beside the old binary'

# Model TERM in the success-to-assignment gap: mv atomically replaces live,
# then reports 143 before INSTALL_BINARY_PROMOTED can be assigned.
TERM_GAP_RUNTIME="$TMP_ROOT/install-term-gap"
make_install_runtime "$TERM_GAP_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$TERM_GAP_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$TERM_GAP_RUNTIME"
    promote_install_routine_asset_surfaces
    promoted_stage="$INSTALL_BINARY_STAGE"
    promoted_live="$INSTALL_BINARY_LIVE"
    mv() {
        if [ "${1:-}" = '-f' ] \
          && [ "${2:-}" = "$promoted_stage" ] \
          && [ "${3:-}" = "$promoted_live" ]; then
            command mv "$@"
            return 143
        fi
        command mv "$@"
    }
    set +e
    promote_install_binary_transaction
    promote_status=$?
    unset -f mv
    _install_cleanup 143
    cleanup_status=$?
    set -e
    [ "$promote_status" -ne 0 ] && [ "$cleanup_status" -eq 143 ]
) || fail_test 'installer did not preserve TERM status across paired recovery'
assert_install_generation "$TERM_GAP_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'TERM after binary rename escaped paired rollback'

# Ordinary post-rename failure and cleanup-file deletion failure both leave a
# completely old pair. Cleanup failure must not undo the paired recovery.
CLEANUP_FAIL_RUNTIME="$TMP_ROOT/install-cleanup-fail"
make_install_runtime "$CLEANUP_FAIL_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$CLEANUP_FAIL_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$CLEANUP_FAIL_RUNTIME"
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    rollback_copy="$INSTALL_BINARY_BACKUP"
    rm() {
        [ "${1:-}" != '-f' ] || [ "${2:-}" != "$rollback_copy" ] || return 74
        command rm "$@"
    }
    set +e
    _install_cleanup 73
    cleanup_status=$?
    set -e
    unset -f rm
    [ "$cleanup_status" -ne 0 ]
) || fail_test 'installer masked cleanup-file deletion failure'
assert_install_generation "$CLEANUP_FAIL_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'cleanup failure broke the recovered binary/assets pair'

# Healthy commit removes rollback generations only after durable commit intent.
INSTALL_RUNTIME="$TMP_ROOT/install-success"
make_install_runtime "$INSTALL_RUNTIME"
printf 'operator routine\n' > "$INSTALL_RUNTIME/routines/operator-private.txt"
printf 'operator helper\n' > "$INSTALL_RUNTIME/routine-helpers/operator-private.txt"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$INSTALL_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$INSTALL_RUNTIME"
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    finalize_install_routine_asset_surfaces
    _install_cleanup 0
)
[ "$(<"$INSTALL_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && grep -Fqx '# new-binary' "$INSTALL_RUNTIME/bin/agentdesk" \
    && [ ! -e "$INSTALL_RUNTIME/routines.old" ] \
    && [ -f "$INSTALL_RUNTIME/routines/operator-private.txt" ] \
    && [ -f "$INSTALL_RUNTIME/routine-helpers/operator-private.txt" ] \
    || fail_test 'installer did not atomically promote and finalize its paired payload'

# Darwin update path: sign the private stage before hashing, clear the previous
# uchg only at the armed rename boundary, and restore both bytes and uchg after
# a post-promotion failure.
IMMUTABLE_RUNTIME="$TMP_ROOT/install-immutable"
make_install_runtime "$IMMUTABLE_RUNTIME"
(
    reset_install_transaction_state
    OS=darwin
    LIVE_FLAG='uchg'
    FLAG_EVENTS=''
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$IMMUTABLE_RUNTIME"
    sign_binary_with_fallback() {
        printf '# signed-stage\n' >> "$1"
    }
    stat() {
        if [ "${1:-}" = '-f' ] && [ "${2:-}" = '%Sf' ]; then
            if [ "${3:-}" = "$IMMUTABLE_RUNTIME/bin/agentdesk" ]; then
                printf '%s\n' "$LIVE_FLAG"
            else
                printf '%s\n' '-'
            fi
            return 0
        fi
        command stat "$@"
    }
    chflags() {
        local operation="$1" path="$2"
        FLAG_EVENTS="${FLAG_EVENTS}${operation}:${path} "
        if [ "$path" = "$IMMUTABLE_RUNTIME/bin/agentdesk" ]; then
            case "$operation" in
                nouchg) LIVE_FLAG='-' ;;
                uchg) LIVE_FLAG='uchg' ;;
                *) return 1 ;;
            esac
        fi
    }
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$IMMUTABLE_RUNTIME"
    [ "$INSTALL_BINARY_NEW_SHA256" = \
        "$(_install_sha256_file "$INSTALL_BINARY_STAGE")" ] \
        && grep -Fq 'signed-stage' "$INSTALL_BINARY_STAGE" \
        && ! grep -Fq 'signed-stage' "$GOOD_ARTIFACT/agentdesk" \
        || exit 95
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    set +e
    _install_cleanup 96
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 96 ] \
        && [ "$LIVE_FLAG" = 'uchg' ] \
        && [[ "$FLAG_EVENTS" == *"nouchg:$IMMUTABLE_RUNTIME/bin/agentdesk "* ]] \
        && [[ "$FLAG_EVENTS" == *"uchg:$IMMUTABLE_RUNTIME/bin/agentdesk "* ]]
) || fail_test 'Darwin staged signing or immutable rollback state diverged'
assert_install_generation "$IMMUTABLE_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'immutable update failure did not restore the old generation'

# Bootstrap can side-effect successfully and still return failure. Cleanup must
# stop that possible new process before restoring the old pair, then restart the
# service that was running before the installer entered the live boundary.
BOOTSTRAP_RUNTIME="$TMP_ROOT/install-bootstrap-gap"
BOOTSTRAP_HOME="$TMP_ROOT/install-bootstrap-home"
make_install_runtime "$BOOTSTRAP_RUNTIME"
mkdir -p "$BOOTSTRAP_HOME/Library/LaunchAgents"
: > "$BOOTSTRAP_HOME/Library/LaunchAgents/com.agentdesk.release.plist"
(
    reset_install_transaction_state
    HOME="$BOOTSTRAP_HOME"
    LAUNCHD_LABEL='com.agentdesk.release'
    SERVICE_ACTIVE=1
    BOOTSTRAP_CALLS=0
    SERVICE_EVENTS=''
    launchd_domain() { printf 'gui/test\n'; }
    launchctl() {
        case "${1:-}" in
            print)
                [ "$SERVICE_ACTIVE" = 1 ] || return 1
                printf '    pid = 5252\n'
                ;;
            bootout)
                SERVICE_ACTIVE=0
                SERVICE_EVENTS="${SERVICE_EVENTS}stop "
                ;;
            bootstrap)
                BOOTSTRAP_CALLS=$((BOOTSTRAP_CALLS + 1))
                SERVICE_ACTIVE=1
                if [ "$BOOTSTRAP_CALLS" -eq 1 ]; then
                    SERVICE_EVENTS="${SERVICE_EVENTS}new-gap "
                    return 143
                fi
                SERVICE_EVENTS="${SERVICE_EVENTS}restart-old "
                ;;
        esac
    }
    adk_process_identity() {
        [ "$1" = 5252 ] || return 1
        printf 'install-candidate-start\n'
    }
    adk_process_instance_alive() {
        [ "$1" = 5252 ] && [ "$2" = install-candidate-start ] \
            && [ "$SERVICE_ACTIVE" = 1 ]
    }
    _install_candidate_port_refuses_connections() {
        [ "$SERVICE_ACTIVE" = 0 ]
    }
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$BOOTSTRAP_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$BOOTSTRAP_RUNTIME"
    # Asset preparation intentionally sources the artifact's shared primitives;
    # reinstall this fixture's deterministic process identity after that source.
    adk_process_identity() {
        [ "$1" = 5252 ] || return 1
        printf 'install-candidate-start\n'
    }
    adk_process_instance_alive() {
        [ "$1" = 5252 ] && [ "$2" = install-candidate-start ] \
            && [ "$SERVICE_ACTIVE" = 1 ]
    }
    stop_install_service_for_promotion
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    set +e
    start_install_service "$HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist"
    start_status=$?
    _install_cleanup 143
    cleanup_status=$?
    set -e
    [ "$start_status" -ne 0 ] \
        && [ "$cleanup_status" -eq 143 ] \
        && [ "$SERVICE_ACTIVE" = 1 ] \
        && [ "$SERVICE_EVENTS" = 'stop new-gap stop restart-old ' ]
) || fail_test 'installer bootstrap side-effect failure escaped paired recovery'
assert_install_generation "$BOOTSTRAP_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'bootstrap failure did not restore the previous install pair'

# launchctl can unload before the old dcserver PID exits. A TERM in that drain
# window must make installer cleanup wait for the captured process instance,
# then restart the untouched old pair.
(
    reset_install_transaction_state
    INSTALL_ROUTINE_ASSET_RUNTIME="$TMP_ROOT/install-drain-runtime"
    INSTALL_LOCK_FILE="$INSTALL_ROUTINE_ASSET_RUNTIME/runtime/deploy-release.lock"
    INSTALL_LOCK_HELD=1
    INSTALL_LAUNCHD_DOMAIN='gui/test'
    INSTALL_SERVICE_WAS_RUNNING=1
    INSTALL_SERVICE_STOP_ATTEMPTED=1
    INSTALL_SERVICE_STOP_CONFIRMED=0
    INSTALL_SERVICE_OLD_PID=4242
    INSTALL_SERVICE_OLD_IDENTITY='old-instance'
    INSTALL_DRAINING=1
    INSTALL_DRAIN_EVENTS=''
    LAUNCHD_LABEL='com.agentdesk.release'
    _install_service_job_is_loaded() { return 1; }
    adk_process_instance_alive() { [ "$INSTALL_DRAINING" = 1 ]; }
    sleep() {
        INSTALL_DRAIN_EVENTS="${INSTALL_DRAIN_EVENTS}drain-wait "
        INSTALL_DRAINING=0
    }
    _adk_active_txn() { return 1; }
    adk_routine_asset_lock_owned() { return 0; }
    adk_release_routine_asset_lock() {
        INSTALL_DRAIN_EVENTS="${INSTALL_DRAIN_EVENTS}release-lock "
    }
    restart_previous_install_service() {
        INSTALL_DRAIN_EVENTS="${INSTALL_DRAIN_EVENTS}restart-old "
    }
    set +e
    _install_cleanup 143
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 143 ] \
        && [ "$INSTALL_SERVICE_STOP_CONFIRMED" = 1 ] \
        && [ "$INSTALL_DRAIN_EVENTS" = \
            'drain-wait restart-old release-lock ' ]
) || fail_test 'installer TERM during old-PID drain stranded the previous service'

# Exact stale-backup sequence: v0/M100 backup survives a healthy v1/M101
# cleanup failure, then v2 still embeds M101 and fails health. The rollback
# guard must read M100 from the digest-bound backup sidecar, never M101 from the
# now-newer live release manifest.
eval "$(extract_function _sha256_file "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _sha256_tree "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _manifest_latest_migration_name "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _manifest_source_git_sha "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _write_rollback_backup_metadata "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _rollback_backup_latest_migration_name "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _latest_postgres_migration_path "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _rollback_would_brick_on_migration "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _migration_seq_from_name "$REPO_ROOT/scripts/_defaults.sh")"
eval "$(extract_function _migration_advanced "$REPO_ROOT/scripts/_defaults.sh")"

MIGRATION_REPO="$TMP_ROOT/migration-repo"
MIGRATION_RUNTIME="$TMP_ROOT/migration-runtime"
mkdir -p "$MIGRATION_REPO/migrations/postgres" "$MIGRATION_RUNTIME/bin" \
    "$MIGRATION_RUNTIME/runtime"
printf '%s\n' '-- M100' > "$MIGRATION_REPO/migrations/postgres/0100_m100.sql"
printf '%s\n' '-- M101' > "$MIGRATION_REPO/migrations/postgres/0101_m101.sql"
seed_live "$MIGRATION_RUNTIME" v0
printf 'v0-binary\n' > "$MIGRATION_RUNTIME/bin/agentdesk.prev"
printf '%s\n' \
    '{"repo_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","latest_postgres_migration":"0100_m100.sql"}' \
    > "$MIGRATION_RUNTIME/runtime/release-source.json"
REL_BINARY_BACKUP="$MIGRATION_RUNTIME/bin/agentdesk.prev"
REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
ROUTINE_ASSET_TXN="$MIGRATION_RUNTIME/runtime/routine-assets.txn.V0"
mkdir -p "$ROUTINE_ASSET_TXN"
ADK_REL="$MIGRATION_RUNTIME" REPO="$MIGRATION_REPO" \
    _write_rollback_backup_metadata \
        "$REL_BINARY_BACKUP" "$REL_BINARY_BACKUP_META" "$ROUTINE_ASSET_TXN"
[ "$(ADK_REL="$MIGRATION_RUNTIME" REPO="$MIGRATION_REPO" \
    _rollback_backup_latest_migration_name)" = '0100_m100.sql' ] \
    || fail_test 'rollback sidecar was not bound to the v0 backup generation'

# v1 became healthy and wrote M101, but its simulated backup cleanup failed.
seed_live "$MIGRATION_RUNTIME" v1
printf 'v1-binary\n' > "$MIGRATION_RUNTIME/bin/agentdesk"
printf '%s\n' \
    '{"repo_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","latest_postgres_migration":"0101_m101.sql"}' \
    > "$MIGRATION_RUNTIME/runtime/release-source.json"
ROUTINE_ASSET_TXN="$MIGRATION_RUNTIME/runtime/routine-assets.txn.V2"
mkdir -p "$ROUTINE_ASSET_TXN"
set +e
ADK_REL="$MIGRATION_RUNTIME" REPO="$MIGRATION_REPO" \
    _rollback_would_brick_on_migration >/dev/null 2>&1
STALE_BACKUP_GUARD_STATUS=$?
set -e
[ "$STALE_BACKUP_GUARD_STATUS" -eq 0 ] \
    || fail_test 'v2 rollback guard trusted the v1 live manifest for stale v0 backup'

# Force may skip migration ordering only after backup integrity succeeds. It
# must never turn a digest-mismatched or metadata-less file into executable
# rollback material.
printf 'tampered-v0-binary\n' > "$REL_BINARY_BACKUP"
set +e
ADK_REL="$MIGRATION_RUNTIME" REPO="$MIGRATION_REPO" \
    AGENTDESK_DEPLOY_FORCE_ROLLBACK=1 \
    _rollback_would_brick_on_migration >/dev/null 2>&1
TAMPERED_BACKUP_GUARD_STATUS=$?
set -e
[ "$TAMPERED_BACKUP_GUARD_STATUS" -eq 0 ] \
    || fail_test 'forced rollback bypassed backup digest verification'

# deploy.sh restart boundaries are independently armed. Model launchctl
# performing bootstrap but returning a signal-like failure before the caller can
# confirm start; cleanup must stop that possible new process, restore the old
# pair, and restart it. A second case covers failure immediately after bootout.
eval "$(extract_function stop_deploy_service_for_rollback "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_service_job_is_running "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_current_service_pid "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _capture_deploy_service_process "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _capture_deploy_candidate_process "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _persist_deploy_candidate_drain_authority "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function capture_deploy_candidate_process_after_start "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_candidate_process_is_alive "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_candidate_port_refuses_connections "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_candidate_drain_is_proven "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function wait_for_deploy_candidate_stop "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _force_stop_deploy_candidate "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function deploy_service_is_running "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function wait_for_deploy_service_stop "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function stop_deploy_service_for_promotion "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function cleanup_deploy_transaction "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function restart_launchd "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_immutable_flag_state "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function _deploy_apply_immutable_flag_state "$REPO_ROOT/scripts/deploy.sh")"
eval "$(extract_function clear_deploy_immutable_flag "$REPO_ROOT/scripts/deploy.sh")"

(
    HOME="$TMP_ROOT/restart-gap-home"
    mkdir -p "$HOME/Library/LaunchAgents"
    : > "$HOME/Library/LaunchAgents/com.agentdesk.release.plist"
    OS=darwin
    LABEL='com.agentdesk.release'
    AD_HOME="$TMP_ROOT/restart-gap-runtime"
    ROUTINE_ASSET_TXN="$AD_HOME/runtime/fake-txn"
    HEALTH_PORT=8791
    DEPLOY_HEALTH_OK=0
    DEPLOY_BINARY_PROMOTED=1
    DEPLOY_RESTART_ARMED=1
    DEPLOY_SERVICE_STOP_ATTEMPTED=0
    DEPLOY_SERVICE_START_ATTEMPTED=0
    DEPLOY_SERVICE_START_CONFIRMED=0
    DEPLOY_SERVICE_STOP_CONFIRMED=1
    DEPLOY_SERVICE_WAS_RUNNING=1
    DEPLOY_SERVICE_CANDIDATE_PID=""
    DEPLOY_SERVICE_CANDIDATE_IDENTITY=""
    DEPLOY_LOCK_HELD=1
    DEPLOY_LOCK_FILE="$AD_HOME/runtime/deploy-release.lock"
    SERVICE_ACTIVE=0
    RESTART_EVENTS=''
    DRAIN_AUTHORITY=0
    adk_persist_routine_asset_candidate_drain_authority() {
        DRAIN_AUTHORITY=1
    }
    adk_routine_asset_candidate_drain_authority_exists() {
        [ "$DRAIN_AUTHORITY" = 1 ]
    }
    adk_clear_routine_asset_candidate_drain_authority() {
        DRAIN_AUTHORITY=0
    }
    _launchd_domain() { printf 'gui/test\n'; }
    _kickstart_launchd_job_if_needed() { :; }
    info() { :; }
    ok() { :; }
    error() { :; }
    sleep() { :; }
    seq() { printf '1\n'; }
    [ "$OS" = darwin ] && [ "$LABEL" = 'com.agentdesk.release' ] \
        || exit 80
    launchctl() {
        case "${1:-}" in
            bootout)
                SERVICE_ACTIVE=0
                RESTART_EVENTS="${RESTART_EVENTS}bootout "
                return 0
                ;;
            bootstrap)
                # Side effect succeeded, but the call reports TERM/failure.
                SERVICE_ACTIVE=1
                RESTART_EVENTS="${RESTART_EVENTS}bootstrap-gap "
                return 143
                ;;
            print)
                [ "$SERVICE_ACTIVE" = 1 ] || return 1
                printf '    pid = 4242\n'
                ;;
        esac
    }
    adk_process_identity() {
        [ "$1" = 4242 ] || return 1
        printf 'candidate-start\n'
    }
    adk_process_instance_alive() {
        [ "$1" = 4242 ] && [ "$2" = candidate-start ] \
            && [ "$SERVICE_ACTIVE" = 1 ]
    }
    _deploy_candidate_port_refuses_connections() {
        [ "$SERVICE_ACTIVE" = 0 ]
    }
    set +e
    restart_launchd
    restart_status=$?
    set -e
    [ "$restart_status" -ne 0 ] \
        && [ "$DEPLOY_SERVICE_STOP_ATTEMPTED" = 1 ] \
        && [ "$DEPLOY_SERVICE_START_ATTEMPTED" = 1 ] \
        && [ "$DEPLOY_SERVICE_START_CONFIRMED" = 0 ] \
        && [ "$SERVICE_ACTIVE" = 1 ] \
        || exit 81

    RESTART_EVENTS=''
    _adk_active_txn() { printf '%s\n' "$AD_HOME/runtime/fake-txn"; }
    adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
    restore_previous_install() {
        RESTART_EVENTS="${RESTART_EVENTS}restore-binary "
    }
    adk_rollback_routine_asset_transaction() {
        RESTART_EVENTS="${RESTART_EVENTS}rollback-assets "
    }
    adk_commit_routine_asset_transaction_forward() { return 82; }
    adk_routine_asset_lock_owned() { return 0; }
    adk_release_routine_asset_lock() {
        RESTART_EVENTS="${RESTART_EVENTS}release-lock "
    }
    cleanup_backup() { RESTART_EVENTS="${RESTART_EVENTS}cleanup-backup "; }
    start_previous_deploy_service() {
        SERVICE_ACTIVE=1
        RESTART_EVENTS="${RESTART_EVENTS}restart-old "
    }
    set +e
    cleanup_deploy_transaction 143
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 143 ] \
        && [ "$SERVICE_ACTIVE" = 1 ] \
        && [ "$RESTART_EVENTS" = \
            'bootout restore-binary rollback-assets restart-old release-lock cleanup-backup ' ]
) || fail_test 'TERM after launchctl bootstrap escaped service-aware paired rollback'

(
    OS=linux
    AD_HOME="$TMP_ROOT/restart-stop-runtime"
    DEPLOY_HEALTH_OK=0
    DEPLOY_BINARY_PROMOTED=1
    DEPLOY_RESTART_ARMED=1
    DEPLOY_SERVICE_STOP_ATTEMPTED=1
    DEPLOY_SERVICE_START_ATTEMPTED=0
    DEPLOY_SERVICE_START_CONFIRMED=0
    DEPLOY_SERVICE_STOP_CONFIRMED=1
    DEPLOY_SERVICE_WAS_RUNNING=1
    DEPLOY_LOCK_HELD=1
    DEPLOY_LOCK_FILE="$AD_HOME/runtime/deploy-release.lock"
    HEALTH_PORT=8791
    STOP_EVENTS=''
    [ "$OS" = linux ] \
        && [ "$DEPLOY_HEALTH_OK" -eq 0 ] \
        && [ "$DEPLOY_BINARY_PROMOTED" -eq 1 ] \
        && [ "$DEPLOY_RESTART_ARMED" -eq 1 ] \
        || exit 84
    error() { :; }
    systemctl() {
        case "$*" in
            *' is-active '*) return 3 ;;
            *' stop '*) STOP_EVENTS="${STOP_EVENTS}stop-new "; return 0 ;;
        esac
    }
    curl() { return 7; }
    _adk_active_txn() { printf '%s\n' "$AD_HOME/runtime/fake-txn"; }
    adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
    restore_previous_install() { STOP_EVENTS="${STOP_EVENTS}restore-binary "; }
    adk_rollback_routine_asset_transaction() { STOP_EVENTS="${STOP_EVENTS}rollback-assets "; }
    adk_commit_routine_asset_transaction_forward() { return 83; }
    adk_routine_asset_lock_owned() { return 0; }
    adk_release_routine_asset_lock() { :; }
    cleanup_backup() { :; }
    start_previous_deploy_service() { STOP_EVENTS="${STOP_EVENTS}restart-old "; }
    set +e
    cleanup_deploy_transaction 75
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 75 ] \
        && [ "$STOP_EVENTS" = \
            'stop-new restore-binary rollback-assets restart-old ' ]
) || fail_test 'failure after service stop left the restored release stopped'

# Exact stop-command/trap boundary: stop completed, but TERM arrived before the
# caller assigned STOP_CONFIRMED. Cleanup must reconcile the inactive service
# and restart the untouched old release even though no binary rename occurred.
(
    OS=linux
    AD_HOME="$TMP_ROOT/stop-confirm-gap-runtime"
    DEPLOY_HEALTH_OK=0
    DEPLOY_BINARY_PROMOTED=0
    DEPLOY_RESTART_ARMED=1
    DEPLOY_SERVICE_STOP_ATTEMPTED=1
    DEPLOY_SERVICE_START_ATTEMPTED=0
    DEPLOY_SERVICE_START_CONFIRMED=0
    DEPLOY_SERVICE_STOP_CONFIRMED=0
    DEPLOY_SERVICE_WAS_RUNNING=1
    DEPLOY_SERVICE_OLD_PID=4242
    DEPLOY_SERVICE_OLD_IDENTITY='old-instance'
    DEPLOY_LOCK_HELD=1
    DEPLOY_LOCK_FILE="$AD_HOME/runtime/deploy-release.lock"
    STOP_GAP_EVENTS=''
    OLD_PROCESS_DRAINING=1
    systemctl() {
        case "$*" in
            *' is-active '*) return 3 ;;
            *) return 88 ;;
        esac
    }
    _adk_active_txn() { return 1; }
    adk_process_instance_alive() { [ "$OLD_PROCESS_DRAINING" = 1 ]; }
    sleep() {
        STOP_GAP_EVENTS="${STOP_GAP_EVENTS}drain-wait "
        OLD_PROCESS_DRAINING=0
    }
    adk_routine_asset_lock_owned() { return 0; }
    adk_release_routine_asset_lock() {
        STOP_GAP_EVENTS="${STOP_GAP_EVENTS}release-lock "
    }
    start_previous_deploy_service() {
        STOP_GAP_EVENTS="${STOP_GAP_EVENTS}restart-old "
    }
    cleanup_backup() { STOP_GAP_EVENTS="${STOP_GAP_EVENTS}cleanup-backup "; }
    error() { :; }
    set +e
    cleanup_deploy_transaction 143
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 143 ] \
        && [ "$DEPLOY_SERVICE_STOP_CONFIRMED" -eq 1 ] \
        && [ "$STOP_GAP_EVENTS" = \
            'drain-wait restart-old release-lock cleanup-backup ' ]
) || fail_test 'TERM after service stop but before confirmation stranded the old release'

# An immutable old binary is cleared only after exact candidate validation and
# asset promotion. If flag clearing fails, no binary rename is attempted;
# cleanup rolls the already-promoted assets back and restarts the old service.
(
    OS=darwin
    AD_HOME="$TMP_ROOT/immutable-clear-runtime"
    DEPLOY_HEALTH_OK=0
    DEPLOY_BINARY_PROMOTED=0
    DEPLOY_RESTART_ARMED=1
    DEPLOY_SERVICE_STOP_ATTEMPTED=1
    DEPLOY_SERVICE_START_ATTEMPTED=0
    DEPLOY_SERVICE_START_CONFIRMED=0
    DEPLOY_SERVICE_STOP_CONFIRMED=1
    DEPLOY_SERVICE_WAS_RUNNING=1
    DEPLOY_SERVICE_OLD_PID=''
    DEPLOY_SERVICE_OLD_IDENTITY=''
    DEPLOY_LOCK_HELD=1
    DEPLOY_LOCK_FILE="$AD_HOME/runtime/deploy-release.lock"
    IMMUTABLE_EVENTS=''
    mkdir -p "$AD_HOME/libexec"
    : > "$AD_HOME/libexec/agentdesk"
    chflags() { return 91; }
    stat() { printf 'uchg\n'; }
    set +e
    clear_deploy_immutable_flag "$AD_HOME/libexec/agentdesk"
    clear_status=$?
    set -e
    [ "$clear_status" -ne 0 ] || exit 92
    _adk_active_txn() { printf '%s\n' "$AD_HOME/runtime/fake-txn"; }
    adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
    adk_rollback_routine_asset_transaction() {
        IMMUTABLE_EVENTS="${IMMUTABLE_EVENTS}rollback-assets "
    }
    adk_commit_routine_asset_transaction_forward() { return 93; }
    adk_routine_asset_lock_owned() { return 0; }
    adk_release_routine_asset_lock() {
        IMMUTABLE_EVENTS="${IMMUTABLE_EVENTS}release-lock "
    }
    start_previous_deploy_service() {
        IMMUTABLE_EVENTS="${IMMUTABLE_EVENTS}restart-old "
    }
    cleanup_backup() { IMMUTABLE_EVENTS="${IMMUTABLE_EVENTS}cleanup-backup "; }
    error() { :; }
    set +e
    cleanup_deploy_transaction "$clear_status"
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq "$clear_status" ] \
        && [ "$IMMUTABLE_EVENTS" = \
            'rollback-assets restart-old release-lock cleanup-backup ' ]
) || fail_test 'immutable clear failure escaped paired asset/service recovery'

# --skip-build may reuse a candidate only with a manifest binding the binary,
# repository HEAD, and both exact asset trees. A semantically valid asset edit
# after the build must still reject the stale generation before any stop/swap.
eval "$(awk '
    /^write_local_build_generation_manifest[(][)] [{]$/ { printing = 1 }
    printing {
        print
        if ($0 == "PY") heredoc_closed = 1
        else if (heredoc_closed && $0 == "}") exit
    }
' "$REPO_ROOT/scripts/build-release.sh")"
eval "$(extract_function validate_local_build_generation_manifest \
    "$REPO_ROOT/scripts/deploy.sh")"
(
    PROJECT_DIR="$TMP_ROOT/local-build-generation"
    mkdir -p "$PROJECT_DIR/target/release" "$PROJECT_DIR/dist" \
        "$PROJECT_DIR/src" "$PROJECT_DIR/migrations/postgres"
    seed_source "$PROJECT_DIR" 'build-v0'
    write_fake_install_candidate "$PROJECT_DIR/target/release/agentdesk"
    printf 'target/\ndist/\n' > "$PROJECT_DIR/.gitignore"
    printf 'pub fn fixture() {}\n' > "$PROJECT_DIR/src/lib.rs"
    printf '%s\n' '-- fixture migration' \
        > "$PROJECT_DIR/migrations/postgres/0100_fixture.sql"
    git -C "$PROJECT_DIR" init -q
    git -C "$PROJECT_DIR" -c user.name=AgentDesk -c user.email=agentdesk@example.invalid \
        add .gitignore src migrations routines routine-helpers
    git -C "$PROJECT_DIR" -c user.name=AgentDesk -c user.email=agentdesk@example.invalid \
        commit -qm 'fixture generation'
    BUILD_SOURCE_SHA="$(git -C "$PROJECT_DIR" rev-parse HEAD)"
    (
        cd "$PROJECT_DIR/dist"
        write_local_build_generation_manifest \
            "$PROJECT_DIR/target/release/agentdesk" "$BUILD_SOURCE_SHA"
    )
    error() { :; }
    validate_local_build_generation_manifest
    printf 'pub fn fixture() { panic!("dirty"); }\n' > "$PROJECT_DIR/src/lib.rs"
    if write_local_build_generation_manifest \
        "$PROJECT_DIR/target/release/agentdesk" "$BUILD_SOURCE_SHA" \
        >/dev/null 2>&1; then
        exit 95
    fi
    printf 'pub fn fixture() {}\n' > "$PROJECT_DIR/src/lib.rs"
    printf '%s\n' '-- dirty migration' \
        > "$PROJECT_DIR/migrations/postgres/0100_fixture.sql"
    if validate_local_build_generation_manifest >/dev/null 2>&1; then
        exit 96
    fi
    printf '%s\n' '-- fixture migration' \
        > "$PROJECT_DIR/migrations/postgres/0100_fixture.sql"
    validate_local_build_generation_manifest
    printf '%s\n' '// evaluator-compatible v1 asset edit' \
        >> "$PROJECT_DIR/routines/monitoring/bundled.js"
    if validate_local_build_generation_manifest >/dev/null 2>&1; then
        exit 94
    fi
    git -C "$PROJECT_DIR" -c user.name=AgentDesk -c user.email=agentdesk@example.invalid \
        add routines
    git -C "$PROJECT_DIR" -c user.name=AgentDesk -c user.email=agentdesk@example.invalid \
        commit -qm 'clean head drift'
    if write_local_build_generation_manifest \
        "$PROJECT_DIR/target/release/agentdesk" "$BUILD_SOURCE_SHA" \
        >/dev/null 2>&1; then
        exit 97
    fi
) || fail_test 'stale skip-build binary was accepted with a different asset generation'
rg -Fq 'BINARY="$PROJECT_DIR/target/release/${BINARY_NAME}"' \
    "$REPO_ROOT/scripts/build-release.sh" \
    || fail_test 'build-release hashes a cwd-relative binary after entering dist'

# An EXIT/TERM trap installed before lock acquisition must be inert with respect
# to another owner's transaction. No marker lookup, rollback, or release is
# authorized until both the local held flag and on-disk token verify.
(
    DEPLOY_HEALTH_OK=0
    DEPLOY_LOCK_HELD=0
    DEPLOY_LOCK_FILE="$TMP_ROOT/prelock-owner/runtime/deploy-release.lock"
    PRELOCK_MUTATION=''
    _adk_active_txn() { PRELOCK_MUTATION="${PRELOCK_MUTATION}inspect "; return 1; }
    adk_routine_asset_lock_owned() { PRELOCK_MUTATION="${PRELOCK_MUTATION}verify "; return 0; }
    adk_release_routine_asset_lock() { PRELOCK_MUTATION="${PRELOCK_MUTATION}release "; }
    cleanup_backup() { :; }
    error() { :; }
    set +e
    cleanup_deploy_transaction 143
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 143 ] && [ -z "$PRELOCK_MUTATION" ]
) || fail_test 'pre-lock deploy cleanup mutated another owner transaction'

# The old service must be confirmed stopped before the first live binary or
# routine asset rename in deploy.sh.
DEPLOY_STOP_LINE="$(awk '/^stop_deploy_service_for_promotion \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
DEPLOY_PREFLIGHT_LINE="$(awk '/^adk_validate_staged_routine_asset_transaction \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
DEPLOY_BINARY_LINE="$(awk '/^mv -f "\$DEPLOY_BINARY_STAGE" "\$REAL_BIN"/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
DEPLOY_ASSET_LINE="$(awk '/^adk_promote_routine_asset_transaction \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
[ -n "$DEPLOY_PREFLIGHT_LINE" ] \
    && [ -n "$DEPLOY_STOP_LINE" ] \
    && [ -n "$DEPLOY_BINARY_LINE" ] \
    && [ -n "$DEPLOY_ASSET_LINE" ] \
    && [ "$DEPLOY_PREFLIGHT_LINE" -lt "$DEPLOY_STOP_LINE" ] \
    && [ "$DEPLOY_STOP_LINE" -lt "$DEPLOY_BINARY_LINE" ] \
    && [ "$DEPLOY_STOP_LINE" -lt "$DEPLOY_ASSET_LINE" ] \
    && [ "$DEPLOY_ASSET_LINE" -lt "$DEPLOY_BINARY_LINE" ] \
    || fail_test 'deploy.sh can mutate a live generation before service stop confirmation'

DEPLOY_GENERATION_LINE="$(awk '/^validate_local_build_generation_manifest \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
DEPLOY_TXN_LINE="$(awk '/^[[:space:]]*adk_begin_routine_asset_transaction "\$AD_HOME"/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy.sh")"
[ -n "$DEPLOY_GENERATION_LINE" ] \
    && [ -n "$DEPLOY_TXN_LINE" ] \
    && [ "$DEPLOY_GENERATION_LINE" -lt "$DEPLOY_TXN_LINE" ] \
    && [ "$DEPLOY_GENERATION_LINE" -lt "$DEPLOY_STOP_LINE" ] \
    || fail_test 'local build generation binding occurs after transaction/service stop'

RELEASE_PREFLIGHT_LINE="$(awk '/^if ! adk_validate_staged_routine_asset_transaction \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy-release.sh")"
RELEASE_MIGRATION_LINE="$(awk '/^_migrate_pg_tunnel_before_release_stop$/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy-release.sh")"
RELEASE_STOP_LINE="$(awk '/^if ! _stop_release_for_promotion; then/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy-release.sh")"
RELEASE_PROMOTE_LINE="$(awk '/^if ! adk_promote_routine_asset_transaction \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/deploy-release.sh")"
[ -n "$RELEASE_PREFLIGHT_LINE" ] \
    && [ "$RELEASE_PREFLIGHT_LINE" -lt "$RELEASE_MIGRATION_LINE" ] \
    && [ "$RELEASE_PREFLIGHT_LINE" -lt "$RELEASE_STOP_LINE" ] \
    && [ "$RELEASE_PREFLIGHT_LINE" -lt "$RELEASE_PROMOTE_LINE" ] \
    || fail_test 'deploy-release exact validation occurs after migration/stop/promotion'

INSTALL_PREFLIGHT_LINE="$(awk '/^adk_validate_staged_routine_asset_transaction \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/install.sh")"
INSTALL_STOP_LINE="$(awk '/^stop_install_service_for_promotion \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/install.sh")"
INSTALL_PROMOTE_LINE="$(awk '/^promote_install_routine_asset_surfaces \\/ { print NR; exit }' \
    "$REPO_ROOT/scripts/install.sh")"
[ -n "$INSTALL_PREFLIGHT_LINE" ] \
    && [ "$INSTALL_PREFLIGHT_LINE" -lt "$INSTALL_STOP_LINE" ] \
    && [ "$INSTALL_PREFLIGHT_LINE" -lt "$INSTALL_PROMOTE_LINE" ] \
    || fail_test 'installer exact validation occurs after service stop/promotion'

# Gitignore keeps operator helpers private while the four bundled files remain
# explicitly trackable.
git -C "$REPO_ROOT" check-ignore --no-index -q -- routine-helpers/operator.py \
    || fail_test 'operator-private helper is not ignored'
for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
    if git -C "$REPO_ROOT" check-ignore --no-index -q -- "routine-helpers/$helper_ref"; then
        fail_test "authoritative helper is ignored: $helper_ref"
    fi
done

# Wiring ratchet ignores comment-only pseudo-calls and proves the shared lexer,
# transaction, lock, health commit, and artifact packaging cannot be removed.
shell_command_count() {
    local source_file="$1"
    local command_name="$2"
    local required_fragment="${3:-$2}"

    awk -v command_name="$command_name" -v fragment="$required_fragment" '
        function inspect(line, first) {
            sub(/^[[:space:]]+/, "", line)
            gsub(/[[:space:]]+/, " ", line)
            if (line == "" || line ~ /^#/) return
            if (line ~ /^[A-Za-z_][A-Za-z0-9_]*[(][)][[:space:]]*[{]/) return
            if (line ~ /^(if|elif|while|until)[[:space:]]+/) {
                sub(/^(if|elif|while|until)[[:space:]]+/, "", line)
            }
            sub(/^![[:space:]]+/, "", line)
            first = line
            sub(/[[:space:];(].*$/, "", first)
            if (first == command_name && index(line, fragment) > 0) count += 1
        }
        {
            if (continued) {
                piece = $0
                sub(/^[[:space:]]+/, "", piece)
                logical = logical " " piece
            } else {
                logical = $0
            }
            if (logical ~ /\\[[:space:]]*$/) {
                sub(/\\[[:space:]]*$/, "", logical)
                continued = 1
                next
            }
            inspect(logical)
            logical = ""
            continued = 0
        }
        END {
            if (logical != "") inspect(logical)
            print count + 0
        }
    ' "$source_file"
}

shell_has_command() {
    [ "$(shell_command_count "$1" "$2" "$3")" -gt 0 ]
}

assert_asset_wiring() {
    local root="$1"
    local prepare_calls

    shell_has_command "$root/scripts/routine-asset-surface.sh" \
        '"$python_bin"' \
        '"$python_bin" "$ADK_QUICKJS_VALIDATOR" "$root"' || return 1
    shell_has_command "$root/scripts/routine-asset-surface.sh" \
        '"$candidate_binary"' \
        '"$candidate_binary" validate-routines --root "$staged_release_root/routines" --runtime-root "$staged_release_root"' \
        || return 1
    shell_has_command "$root/scripts/deploy-release.sh" \
        'adk_acquire_routine_asset_lock' \
        'adk_acquire_routine_asset_lock "$DEPLOY_LOCK_FILE"' || return 1
    shell_has_command "$root/scripts/deploy-release.sh" \
        '_acquire_release_deploy_lock' '_acquire_release_deploy_lock "$@"' \
        || return 1
    shell_has_command "$root/scripts/deploy-release.sh" \
        'adk_promote_routine_asset_transaction' \
        'adk_promote_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN" "$STAGED_BINARY"' \
        || return 1
    shell_has_command "$root/scripts/deploy.sh" \
        'adk_promote_routine_asset_transaction' \
        'adk_promote_routine_asset_transaction "$AD_HOME" "$ROUTINE_ASSET_TXN" "$DEPLOY_BINARY_STAGE"' \
        || return 1
    shell_has_command "$root/scripts/install.sh" \
        'adk_promote_routine_asset_transaction' \
        'adk_promote_routine_asset_transaction "$INSTALL_ROUTINE_ASSET_RUNTIME" "$INSTALL_ROUTINE_ASSET_TXN" "$INSTALL_BINARY_STAGE"' \
        || return 1
    shell_has_command "$root/scripts/deploy-release.sh" \
        'adk_commit_routine_asset_transaction' \
        'adk_commit_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN"' \
        || return 1
    shell_has_command "$root/scripts/deploy.sh" \
        'DEPLOY_HEALTH_OK=1' 'DEPLOY_HEALTH_OK=1' || return 1
    shell_has_command "$root/scripts/deploy.sh" \
        'adk_commit_routine_asset_transaction' \
        'adk_commit_routine_asset_transaction "$AD_HOME" "$ROUTINE_ASSET_TXN"' \
        || return 1
    shell_has_command "$root/scripts/build-release.sh" 'cp' \
        'cp "scripts/validate-quickjs-routines.py" "$STAGING/scripts/validate-quickjs-routines.py"' \
        || return 1
    shell_has_command "$root/scripts/build-release.sh" \
        'write_local_build_generation_manifest' \
        'write_local_build_generation_manifest "$BINARY"' || return 1
    prepare_calls="$(shell_command_count "$root/scripts/install.sh" \
        'prepare_install_routine_asset_surfaces' \
        'prepare_install_routine_asset_surfaces')"
    [ "$prepare_calls" -eq 1 ] || return 1
    if [ "$(shell_command_count "$root/scripts/deploy-release.sh" 'rsync' \
        '--delete "$REPO/routines/"')" -gt 0 ]; then
        return 1
    fi
}

assert_asset_wiring "$REPO_ROOT" || fail_test 'production asset wiring is incomplete'
MUTATION_ROOT="$TMP_ROOT/wiring-mutation"
mkdir -p "$MUTATION_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh,routine-asset-surface.sh,validate-quickjs-routines.py} \
    "$MUTATION_ROOT/scripts/"
awk '
    /^[[:space:]]*prepare_install_routine_asset_surfaces / {
        print "  # " $0
        next
    }
    { print }
' "$MUTATION_ROOT/scripts/install.sh" > "$MUTATION_ROOT/scripts/install.sh.mutated"
mv "$MUTATION_ROOT/scripts/install.sh.mutated" "$MUTATION_ROOT/scripts/install.sh"
if assert_asset_wiring "$MUTATION_ROOT"; then
    fail_test 'wiring ratchet accepted comment-only installer calls'
fi

comment_first_executable_fragment() {
    local source_file="$1"
    local fragment="$2"
    local mutated_file="${source_file}.mutated"

    awk -v fragment="$fragment" '
        {
            executable = $0
            sub(/^[[:space:]]+/, "", executable)
            if (!done && executable !~ /^#/ && index(executable, fragment) > 0) {
                print "# COMMENTED-OUT MUTANT: " $0
                done = 1
                next
            }
            print
        }
        END { if (!done) exit 92 }
    ' "$source_file" > "$mutated_file" || return 1
    mv "$mutated_file" "$source_file"
}

# Regression for the exact weak-ratchet defect: leaving the full call text in
# a comment must not satisfy deploy-release, deploy.sh, or artifact packaging.
WIRING_MUTANT_FILES=(
    'routine-asset-surface.sh'
    'deploy-release.sh'
    'deploy.sh'
    'build-release.sh'
)
WIRING_MUTANT_FRAGMENTS=(
    '"$candidate_binary" validate-routines'
    'adk_commit_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN"'
    'adk_commit_routine_asset_transaction "$AD_HOME" "$ROUTINE_ASSET_TXN"'
    'cp "scripts/validate-quickjs-routines.py" "$STAGING/scripts/validate-quickjs-routines.py"'
)
for mutant_index in "${!WIRING_MUTANT_FILES[@]}"; do
    EXEC_MUTATION_ROOT="$TMP_ROOT/executable-wiring-mutation-$mutant_index"
    mkdir -p "$EXEC_MUTATION_ROOT/scripts"
    cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh,routine-asset-surface.sh,validate-quickjs-routines.py} \
        "$EXEC_MUTATION_ROOT/scripts/"
    comment_first_executable_fragment \
        "$EXEC_MUTATION_ROOT/scripts/${WIRING_MUTANT_FILES[$mutant_index]}" \
        "${WIRING_MUTANT_FRAGMENTS[$mutant_index]}" \
        || fail_test "could not create executable wiring mutant $mutant_index"
    if assert_asset_wiring "$EXEC_MUTATION_ROOT"; then
        fail_test "wiring ratchet accepted commented ${WIRING_MUTANT_FILES[$mutant_index]} call"
    fi
done

LEXER_MUTATION_ROOT="$TMP_ROOT/lexer-wiring-mutation"
mkdir -p "$LEXER_MUTATION_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh,routine-asset-surface.sh,validate-quickjs-routines.py} \
    "$LEXER_MUTATION_ROOT/scripts/"
awk 'index($0, "\"$python_bin\" \"$ADK_QUICKJS_VALIDATOR\" \"$root\"") == 0' \
    "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh" \
    > "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh.mutated"
mv "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh.mutated" \
    "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh"
if assert_asset_wiring "$LEXER_MUTATION_ROOT"; then
    fail_test 'wiring ratchet missed lexical validator removal'
fi

echo 'routine helper asset transaction tests passed'
