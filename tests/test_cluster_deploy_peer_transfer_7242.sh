#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$ROOT/scripts/deploy-release.sh}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/peer-transfer-test.XXXXXX")"
trap 'rm -r "$WORK"' EXIT
export WORK DEPLOY_SH
export PATH="$WORK/tools:$PATH"
mkdir -p "$WORK/tools" "$WORK/leader/bin" "$WORK/peer/bin" "$WORK/repo"
export TRACE="$WORK/trace" PEER_ROOT="$WORK/peer"
export EXPECTED_SHA=0123456789abcdef0123456789abcdef01234567

extract_function() {
    awk -v start="^$1[(][)] [{]$" '
        $0 ~ start { printing=1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}
fail() { echo "FAIL: $*" >&2; exit 1; }
for fn in _peer_binary_platform _peer_binary_platform_difference _peer_binary_signature _verify_peer_binary \
    _prepare_peer_binary _select_peer_binary _cleanup_peer_binary _cleanup_peer_binary_on_peer \
    _deploy_peer_env_prelude _deploy_to_one_peer _spawn_detached_helper; do
    body="$(extract_function "$fn")"
    [ -z "$body" ] || eval "$body"
    printf '%s\n' "$body" >>"$WORK/functions.sh"
done

cat >"$WORK/tools/uname" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in -m) echo "${FAKE_ARCH:-arm64}";; *) echo Darwin;; esac
STUB
cat >"$WORK/tools/rustc" <<'STUB'
#!/usr/bin/env bash
echo "rustc ${FAKE_RUSTC:-1.92.0}"
STUB
cat >"$WORK/tools/sw_vers" <<'STUB'
#!/usr/bin/env bash
echo "${FAKE_MACOS:-15.6}"
STUB
cat >"$WORK/tools/xcrun" <<'STUB'
#!/usr/bin/env bash
echo 15.5
STUB
cat >"$WORK/tools/codesign" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
    --verify) [ "${FAKE_SIGNATURE:-ok}" != bad ] ;;
    -dvv|-dvvv*)
        printf '%s\n' 'Identifier=com.itismyfield.agentdesk' 'Authority=Developer ID Application: fixture' >&2
        case "$1" in
            -dvvv*) printf '%s\n' 'CandidateCDHash sha256=0123456789abcdef' 'CDHash=0123456789abcdef' 'CMSDigest=fixture' >&2 ;;
        esac
        ;;
    *) exit 2 ;;
esac
STUB
cat >"$WORK/tools/cargo" <<'STUB'
#!/usr/bin/env bash
echo "${BUILD_NODE:-build}" >>"$TRACE"
cp "$WORK/good-binary" "$WORK/built"
chmod +x "$WORK/built"
STUB
cat >"$WORK/tools/ssh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
remote="${*: -1}"
remote="${remote#bash -lc }"
remote="$(eval "printf '%s' $remote")"
case "$remote" in
    *'git merge'*) echo presync >>"$TRACE"; exit 0 ;;
    *_extract_yaml_server_port_shell*) printf '%s\n' "$PEER_ROOT" 18742; exit 0 ;;
esac
export FAKE_ARCH="${PEER_ARCH:-arm64}" FAKE_RUSTC="${PEER_RUSTC:-1.92.0}" FAKE_MACOS="${PEER_MACOS:-15.6}"
export FAKE_SIGNATURE="${PEER_SIGNATURE:-ok}"
case "$remote" in
    *'bash scripts/deploy-release.sh'*)
        [ "${LAUNCH_FAIL:-0}" != 1 ] || exit 255
        export FAKE_MACOS="${PEER_RUNTIME_MACOS:-$FAKE_MACOS}" BUILD_NODE=build
        ;;
    *_cleanup_peer_binary*) echo remote-cleanup >>"$TRACE" ;;
esac
# Keep PATH stubs active; a login shell would replace PATH with host tools.
bash -c "$remote"
STUB
cat >"$WORK/tools/scp" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
echo transfer >>"$TRACE"
while [ "${1:-}" = -o ]; do shift 2; done
source=$1 destination=${2#peer:}
if [ "${TRANSFER_FAIL:-0}" = 1 ]; then
    printf partial >"$destination"
    exit 1
fi
cp "$source" "$destination"
case "$destination" in
    */manifest.json)
        case "${CORRUPT:-}" in
            format|source|profile|signature-meta)
                python3 - "$destination" "$CORRUPT" <<'PY'
import json
import sys
from pathlib import Path
p = Path(sys.argv[1])
m = json.loads(p.read_text())
field, value = {'format': ('format', 0), 'source': ('source_sha', 'old'),
                'profile': ('profile', 'release-fast'),
                'signature-meta': ('signature', 'old-signature')}[sys.argv[2]]
m[field] = value
p.write_text(json.dumps(m))
PY
                ;;
        esac ;;
    */agentdesk)
        case "${CORRUPT:-}" in
            hash) python3 - "$destination" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
p.write_bytes(p.read_bytes().replace(b'fixture', b'fixturz'))
PY
                ;;
            size) printf extra >>"$destination" ;;
            execute) chmod -x "$destination" ;;
            runtime) sed 's/exit 0/exit 8/' "$destination" >"$destination.new"; mv "$destination.new" "$destination" ;;
        esac ;;
esac
STUB
chmod +x "$WORK/tools/"*
cat >"$WORK/good-binary" <<'BIN'
#!/usr/bin/env bash
[ "${1:-}" = build-info ] || exit 9
printf '{"fixture": true}\n'
exit 0
BIN
chmod +x "$WORK/good-binary"

# The peer driver evaluates the production binary-selection/build/staging blocks.
python3 - "$DEPLOY_SH" "$WORK" <<'PY'
import sys
from pathlib import Path
source = Path(sys.argv[1]).read_text()
work = Path(sys.argv[2])
build = source.split('# Build the release binary from HEAD by default;', 1)[1]
build = '# Build the release binary from HEAD by default;' + build.split('# The binary itself reports its O ledger reader;', 1)[0]
(work / 'build-block').write_text(build)
stage = source.split('STAGED_BINARY="$(_staged_deploy_binary_path)"', 1)[1]
stage = 'STAGED_BINARY="$(_staged_deploy_binary_path)"' + stage.split('sign_binary_with_fallback "$STAGED_BINARY"', 1)[0] + 'sign_binary_with_fallback "$STAGED_BINARY"\n'
(work / 'stage-block').write_text(stage)
leader_stage = source.split('STAGED_BINARY="$(_staged_deploy_binary_path)"', 1)[1]
leader_stage = 'STAGED_BINARY="$(_staged_deploy_binary_path)"' + leader_stage.split('_clean_release_build_cache_after_staging', 1)[0]
(work / 'leader-stage-block').write_text(leader_stage)
promote = source.split('mv -f "$STAGED_BINARY" "$REL_BINARY"', 1)[1]
promote = 'mv -f "$STAGED_BINARY" "$REL_BINARY"' + promote.split('# Swap policies with nothing fallible', 1)[0]
(work / 'leader-promote-block').write_text(promote)
smoke = source.split('# >>> BEGIN smoke-disposition region', 1)[1].split('\n', 1)[1]
(work / 'smoke-block').write_text(smoke.split('# <<< END smoke-disposition region', 1)[0])
PY
mkdir -p "$WORK/repo/scripts"
cat >"$WORK/repo/scripts/build_token.py" <<'PY'
import subprocess
import sys
sys.exit(subprocess.call(sys.argv[sys.argv.index('--') + 1:]))
PY
cat >"$WORK/defaults.sh" <<'STUB'
_preflight_resource_contention() { :; }
STUB
cp "$WORK/defaults.sh" "$WORK/repo/scripts/_defaults.sh"
cat >"$WORK/repo/scripts/deploy-release.sh" <<'DRIVER'
#!/usr/bin/env bash
set -euo pipefail
. "$WORK/functions.sh"
REPO="$WORK/repo" ADK_REL="$PEER_ROOT" SCRIPT_DIR="$WORK/repo/scripts"
DEPLOY_PEER_INVOCATION=1 DEPLOY_TARGET_SHA="$EXPECTED_SHA" DEPLOY_BUILD_PROFILE=release
DEPLOY_LOCK_TIMEOUT_SECS=1 DEPLOY_BUILT_SOURCE_SHA="" DEPLOY_RUN_STARTED="$SECONDS"
if declare -F _cleanup_peer_binary >/dev/null; then trap _cleanup_peer_binary EXIT; fi
_ensure_dashboard_dependencies() { :; }
_check_repo_remote_freshness() { :; }
_external_artifact_would_skip_o_writer() { return 1; }
_resolve_default_release_binary() { echo "$WORK/built"; }
_schema_order_peers() { :; }
_staged_deploy_binary_path() { echo "$PEER_ROOT/bin/staged"; }
sign_binary_with_fallback() { echo sign >>"$TRACE"; }
xattr() { :; }
git() { echo "${PEER_RUNTIME_HEAD:-$EXPECTED_SHA}"; }
. "$WORK/build-block"
. "$WORK/stage-block"
mv "$STAGED_BINARY" "$PEER_ROOT/bin/agentdesk"
echo activate >>"$TRACE"
_run_post_deploy_functional_smoke() { echo peer-smoke >>"$TRACE"; }
POST_DEPLOY_SMOKE_WEDGE_COVERAGE=clean POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE=clean
POST_DEPLOY_SMOKE_DURABLE_COVERAGE=clean POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE=clean
POST_DEPLOY_SMOKE_EVIDENCE=fixture
. "$WORK/smoke-block"
DRIVER
chmod +x "$WORK/repo/scripts/deploy-release.sh"
cat >"$WORK/leader-driver.sh" <<'DRIVER'
#!/usr/bin/env bash
set -euo pipefail
. "$WORK/functions.sh"
REPO="$WORK/repo" ADK_REL="$WORK/leader" SCRIPT_DIR="$WORK/repo/scripts"
DEPLOY_PEER_INVOCATION=0 DEPLOY_TARGET_SHA="$EXPECTED_SHA" DEPLOY_BUILD_PROFILE=release
DEPLOY_LOCK_TIMEOUT_SECS=1 DEPLOY_BUILT_SOURCE_SHA="" DEPLOY_RUN_STARTED="$SECONDS"
DEPLOY_SSH_CONNECT_TIMEOUT=1
export BUILD_NODE=leader-build
unset DEPLOY_BINARY_PLATFORM DEPLOY_PEER_BINARY_SOURCE
_ensure_dashboard_dependencies() { :; }
_check_repo_remote_freshness() { :; }
_external_artifact_would_skip_o_writer() { return 1; }
_resolve_default_release_binary() { echo "$WORK/built"; }
_schema_order_peers() { echo peer; }
_staged_deploy_binary_path() { echo "$ADK_REL/bin/staged"; }
sign_binary_with_fallback() { echo leader-sign >>"$TRACE"; }
xattr() { :; }
git() { echo "$EXPECTED_SHA"; }
_extract_yaml_server_port_shell() { :; }
_wait_for_peer_deploy_verdict() { echo peer-health >>"$TRACE"; }
. "$WORK/build-block"
STAGING_PHASE_STARTED="$SECONDS"
. "$WORK/leader-stage-block"
if [ "$1" = leader-live ]; then
    REL_BINARY="$ADK_REL/bin/agentdesk"
    . "$WORK/leader-promote-block"
fi
echo leader-ready >>"$TRACE"
_deploy_to_one_peer peer
DRIVER

# shellcheck disable=SC2329
_extract_yaml_server_port_shell() { :; }
# shellcheck disable=SC2329
_wait_for_peer_deploy_verdict() {
    [ "$1" = peer ] && [ "$4" = 18742 ] && [ "$5" = "$EXPECTED_SHA" ] || return 1
    echo peer-health >>"$TRACE"
}
# shellcheck disable=SC2329
git() { echo "$EXPECTED_SHA"; }

# shellcheck disable=SC2034  # Fixtures are consumed by the extracted production functions.
run_case() (
    local scenario=$1
    export PEER_ARCH=arm64 PEER_RUSTC=1.92.0 PEER_MACOS=15.6 PEER_SIGNATURE=ok
    export TRANSFER_FAIL=0 CORRUPT="" LAUNCH_FAIL=0
    unset PEER_RUNTIME_MACOS PEER_RUNTIME_HEAD
    unset AGENTDESK_DEPLOY_BINARY AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR
    case "$scenario" in
        success|codesign-verbosity|stages|concurrency|detach|leader-staged|leader-live|unavailable) ;;
        launch-fail) export LAUNCH_FAIL=1 ;;
        peer-platform) export PEER_RUNTIME_MACOS=14.0 ;;
        head-target) export PEER_RUNTIME_HEAD=old-head ;;
        hash|size|execute|runtime|format|source|profile|signature-meta) export CORRUPT="$scenario" ;;
        optimized-hash) export CORRUPT=hash PYTHONOPTIMIZE=1 ;;
        arch) export PEER_ARCH=x86_64 ;;
        toolchain) export PEER_RUSTC=1.91.0 ;;
        os) export PEER_MACOS=14.0 ;;
        signature) export PEER_SIGNATURE=bad ;;
        transfer) export TRANSFER_FAIL=1 ;;
        *) fail "unknown case $scenario" ;;
    esac
    : >"$TRACE"
    cp "$WORK/good-binary" "$WORK/leader/bin/signed"
    REPO="$WORK/repo" ADK_REL="$WORK/leader"
    DEPLOY_PEER_BINARY_SOURCE="$ADK_REL/bin/signed"
    DEPLOY_BUILT_SOURCE_SHA="$EXPECTED_SHA" DEPLOY_TARGET_SHA="$EXPECTED_SHA"
    DEPLOY_BUILD_PROFILE=release DEPLOY_SSH_CONNECT_TIMEOUT=1
    if [ "$scenario" = codesign-verbosity ]; then
        local details verbosity
        details="$(codesign -dvv "$DEPLOY_PEER_BINARY_SOURCE" 2>&1)"
        ! printf '%s\n' "$details" | grep -q '^CDHash=' || fail "codesign -dvv unexpectedly emitted CDHash"
        for verbosity in -dvvv -dvvvv; do
            details="$(codesign "$verbosity" "$DEPLOY_PEER_BINARY_SOURCE" 2>&1)"
            printf '%s\n' "$details" | grep -q '^CDHash=.' || fail "codesign $verbosity omitted CDHash"
        done
    fi
    if declare -F _peer_binary_platform >/dev/null; then
        DEPLOY_BINARY_PLATFORM="$(_peer_binary_platform)"
    fi
    export AGENTDESK_PEER_REPO_DIR="$WORK/repo"
    if [ "$scenario" = concurrency ]; then
        _prepare_peer_binary peer "cd $WORK/repo" "$PEER_ROOT" "$EXPECTED_SHA" >"$WORK/$scenario.log" 2>&1
        eval "$PEER_BINARY_ENV"
        local first="$AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR"
        _prepare_peer_binary peer "cd $WORK/repo" "$PEER_ROOT" "$EXPECTED_SHA" >>"$WORK/$scenario.log" 2>&1
        eval "$PEER_BINARY_ENV"
        local second="$AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR"
        [ "$first" != "$second" ] || fail "transfers shared a directory"
        _verify_peer_binary "$first" "$EXPECTED_SHA"
        _verify_peer_binary "$second" "$EXPECTED_SHA"
        ADK_REL="$PEER_ROOT" AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR="$first"
        _cleanup_peer_binary
        [ -f "$second/agentdesk" ] || fail "cleanup removed another transfer"
        AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR="$second"
        _cleanup_peer_binary
        echo "PASS $scenario"
        exit 0
    fi
    if [ "$scenario" = detach ]; then
        mkdir -p "$WORK/helper"
        cat >"$WORK/helper/deploy-release.sh" <<'PROBE'
#!/usr/bin/env bash
printf '%s\n' "${AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR:-}" >"$WORK/helper-result"
PROBE
        chmod +x "$WORK/helper/deploy-release.sh"
        # shellcheck disable=SC2329
        tmux() { printf '%s\n' "${*: -1}" >"$WORK/helper-path"; }
        SCRIPT_DIR="$WORK/helper" REPORT_CHANNEL_ID=1 REPORT_PROVIDER=fixture
        DEPLOY_DELAY_SECS=0 DEPLOY_TEST_MODE=0 POST_DEPLOY_SMOKE_SCOPE=full
        DEPLOY_LOCK_FILE="$WORK/lock" DEPLOY_LOCK_TIMEOUT_SECS=1
        DEPLOY_PEER_VERDICT_TIMEOUT_SECS=1 DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
        BUNDLE_ID=fixture PLIST_REL=fixture
        AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR="$PEER_ROOT/bin/.peer-transfer.123456"
        _spawn_detached_helper >"$WORK/$scenario.log" 2>&1
        env -u AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR bash "$(cat "$WORK/helper-path")"
        [ "$(cat "$WORK/helper-result")" = "$AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR" ] || fail "detached helper lost peer artifact"
        echo "PASS $scenario"
        exit 0
    fi
    local deploy_rc=0
    case "$scenario" in
        leader-staged|leader-live)
            bash "$WORK/leader-driver.sh" "$scenario" >"$WORK/$scenario.log" 2>&1 || deploy_rc=$?
            ;;
        *)
            [ "$scenario" != unavailable ] || unset DEPLOY_PEER_BINARY_SOURCE
            echo leader-smoke >>"$TRACE"
            _deploy_to_one_peer peer >"$WORK/$scenario.log" 2>&1 || deploy_rc=$?
            ;;
    esac
    if [ "$scenario" = launch-fail ]; then
        [ "$deploy_rc" != 0 ] || fail "$scenario reported success"
        grep -qx transfer "$TRACE" || fail "$scenario did not transfer before launch"
        grep -qx remote-cleanup "$TRACE" || fail "$scenario did not clean up remotely"
        [ -z "$(find "$PEER_ROOT/bin" -name '.peer-transfer.*' -print)" ] || fail "$scenario left transfer staging"
        ! grep -Eq '^(activate|peer-health|build)$' "$TRACE" || fail "$scenario continued after failed launch"
        if [ -n "${AGENTDESK_TEST_LOG_DIR:-}" ]; then
            mkdir -p "$AGENTDESK_TEST_LOG_DIR"
            cp "$WORK/$scenario.log" "$AGENTDESK_TEST_LOG_DIR/$scenario.log"
            cp "$TRACE" "$AGENTDESK_TEST_LOG_DIR/$scenario.trace"
        fi
        echo "PASS $scenario"
        exit 0
    fi
    [ "$deploy_rc" = 0 ] || fail "$scenario deploy failed: $(cat "$WORK/$scenario.log")"
    grep -qx activate "$TRACE" || fail "$scenario did not activate"
    grep -qx peer-smoke "$TRACE" || fail "$scenario skipped peer smoke"
    grep -qx peer-health "$TRACE" || fail "$scenario skipped peer health"
    if [ "$scenario" = success ] || [ "$scenario" = codesign-verbosity ] || [ "$scenario" = stages ] || [[ "$scenario" == leader-* ]]; then
        grep -qx transfer "$TRACE" || fail "$scenario did not transfer"
        ! grep -qx build "$TRACE" || fail "$scenario rebuilt on peer"
        cmp -s "$WORK/good-binary" "$PEER_ROOT/bin/agentdesk" || fail "$scenario activated different bytes"
        grep -q 'peer-binary.*verified' "$WORK/$scenario.log" || fail "$scenario missing verification log"
    else
        [ "$(grep -cx build "$TRACE")" = 1 ] || fail "$scenario did not build exactly once"
        [ "$(grep -c 'peer-binary.*fallback' "$WORK/$scenario.log")" = 1 ] || fail "$scenario missing single fallback log"
    fi
    case "$scenario" in
        arch|toolchain|os) ! grep -qx transfer "$TRACE" || fail "$scenario transferred incompatible binary" ;;
    esac
    case "$scenario" in
        os)
            grep -E 'fallback:.*macos.*15[.]6.*14[.]0' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing differing macos values in fallback"
            ;;
        toolchain)
            grep -E 'fallback:.*rustc.*1[.]92[.]0.*1[.]91[.]0' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing differing rustc values in fallback"
            ;;
        signature)
            grep -E 'fallback:.*signature' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing signature reason in fallback"
            ;;
        peer-platform)
            grep -E 'fallback:.*platform.*manifest=.*macos.*15[.]6.*peer=.*14[.]0' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing manifest/peer platform difference"
            ;;
        head-target)
            grep -E 'fallback:.*head.*old-head.*target=' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing head/target reason"
            ;;
        unavailable)
            grep -E 'fallback:.*artifact unavailable' "$WORK/$scenario.log" >/dev/null || fail "$scenario missing artifact reason"
            ;;
    esac
    python3 - "$TRACE" "$scenario" <<'PY'
import sys
from pathlib import Path
trace = Path(sys.argv[1]).read_text().splitlines()
first = 'leader-ready' if sys.argv[2].startswith('leader-') else 'leader-smoke'
assert trace.index(first) < trace.index('presync') < trace.index('activate') < trace.index('peer-smoke') < trace.index('peer-health'), trace
PY
    [ -z "$(find "$PEER_ROOT/bin" -name '.peer-transfer.*' -print)" ] || fail "$scenario left transfer staging"
    if [ -n "${AGENTDESK_TEST_LOG_DIR:-}" ]; then
        mkdir -p "$AGENTDESK_TEST_LOG_DIR"
        cp "$WORK/$scenario.log" "$AGENTDESK_TEST_LOG_DIR/$scenario.log"
        cp "$TRACE" "$AGENTDESK_TEST_LOG_DIR/$scenario.trace"
    fi
    echo "PASS $scenario"
)

if [ "$#" -gt 0 ]; then
    run_case "$1"
else
    count=0
    for scenario in success codesign-verbosity hash arch transfer stages size signature execute runtime toolchain os format source profile signature-meta optimized-hash concurrency detach leader-staged leader-live unavailable launch-fail peer-platform head-target; do
        run_case "$scenario"
        count=$((count + 1))
    done
    echo "peer transfer: $count passed, 0 failed, 0 skipped"
fi
