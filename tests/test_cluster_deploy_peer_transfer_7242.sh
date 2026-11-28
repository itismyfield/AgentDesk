#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$ROOT/scripts/deploy-release.sh}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/peer-transfer-test.XXXXXX")"
trap 'rm -r "$WORK"' EXIT
export WORK DEPLOY_SH
export TMPDIR="$WORK"
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
    _deploy_peer_env_prelude _deploy_to_one_peer _spawn_detached_helper \
    _emit_terminal_deploy_marker; do
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
if [ "${1:-}" = --edition ]; then
    [ "$PWD" = "$FIXTURE_TOOLCHAIN_ROOT" ] && [ -f rust-toolchain.toml ] || exit 92
    [ "${*: -2:1}" = -o ] || exit 93
    cat >"${*: -1}" <<'RESOLVER'
#!/usr/bin/env bash
[ "$PWD" = "$FIXTURE_SERVICE_CWD" ] || exit 94
printf '%s\n' "${AGENTDESK_CONFIG:-$1/config/agentdesk.yaml}"
RESOLVER
    chmod +x "${*: -1}"
    exit 0
fi
echo "rustc ${FAKE_RUSTC:-1.92.0}"
echo "host: aarch64-apple-darwin"
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
    *'git merge'*)
        echo presync >>"$TRACE"
        guard='if [ ! -r scripts/lib/deploy_peers_only.sh'
        if [ "${OLD_PEER_CHECKOUT:-0}" = 1 ] && [[ "$remote" == *"$guard"* ]]; then
            cd "$WORK/peer"
            exec bash -c "$guard${remote##*"$guard"}"
        fi
        exit 0
        ;;
    *_extract_yaml_server_port_shell*) printf '%s\n' "$PEER_ROOT" 18742; exit 0 ;;
esac
export FAKE_ARCH="${PEER_ARCH:-arm64}" FAKE_RUSTC="${PEER_RUSTC:-1.92.0}" FAKE_MACOS="${PEER_MACOS:-15.6}"
export FAKE_SIGNATURE="${PEER_SIGNATURE:-ok}"
case "$remote" in
    *'bash scripts/deploy-release.sh'*)
        [ "${LAUNCH_FAIL:-0}" != 1 ] || exit 255
        if [ -n "${EXPECTED_PEER_FALLBACK:-}" ] \
            && [[ "$remote" != *" AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK=$EXPECTED_PEER_FALLBACK "* ]]; then
            exit 2
        fi
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
            format|source|profile|signature-meta|witness-mismatch)
                python3 - "$destination" "$CORRUPT" <<'PY'
import json
import sys
from pathlib import Path
p = Path(sys.argv[1])
m = json.loads(p.read_text())
if sys.argv[2] == 'witness-mismatch':
    m['running_source']['sha256'] = '0' * 64
else:
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
if [ "${1:-}" = doctor ]; then
    route="${FAKE_RELEASE_DB_ROUTE:-}"
    if [ -n "${AGENTDESK_CONFIG:-}" ]; then
        [ -f "$AGENTDESK_CONFIG" ] || exit 7
        selected="$(sed -n 's/^# fixture DB: //p' "$AGENTDESK_CONFIG")"
        route="${selected:-$route}"
    fi
    printf 'doctor\ndoctor-route %s\n' "$route" >>"$TRACE"
    [ "$AGENTDESK_ROOT_DIR" = "$WORK/leader" ] || exit 7
    case "$route" in
        db-pending) FAKE_PENDING='[2]' ;;
        db-current) FAKE_PENDING='[]' ;;
        primary) ;;
        *) exit 7 ;;
    esac
    printf '{"checks":[{"id":"postgres_connection","evidence":{"applied_count":1,"resolved_count":1,"pending_versions":%s,"checksum_mismatches":%s}}]}\n' \
        "${FAKE_PENDING:-[]}" "${FAKE_CHECKSUM_MISMATCHES:-[]}"
    exit 0
fi
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
stage_start = 'STAGED_BINARY="$(_staged_deploy_binary_path)"'
stage = stage_start + source.split(stage_start, 1)[1]
(work / 'stage-block').write_text(stage.split('_cleanup_peer_binary', 1)[0])
(work / 'leader-stage-block').write_text(stage.split('_clean_release_build_cache_after_staging', 1)[0])
promote_start = 'rm -f "$REL_BINARY.source.json"'
promote = promote_start + source.split(promote_start, 1)[1]
(work / 'leader-promote-block').write_text(promote.split('# Swap policies with nothing fallible', 1)[0])
smoke = source.split('# >>> BEGIN smoke-disposition region', 1)[1].split('\n', 1)[1]
(work / 'smoke-block').write_text(smoke.split('# <<< END smoke-disposition region', 1)[0])
# Retain the entry point and dispatch, but end before any actual local staging.
prefix = source.split('# Stage agent prompt files', 1)[0]
entry = 'if _self_hosted_release_session; then'
dry = 'if [ "$DEPLOY_TEST_MODE" = "1" ]; then'
assert prefix.count(entry) == prefix.count(dry) == 1
prefix = prefix.replace(entry, '. "$WORK/entry-stubs.sh"\n' + entry)
# Within this isolated prefix, fake work replaces activation and all resource probes.
prefix = prefix.replace(dry, 'if false; then')
prefix = prefix.replace('if [ "$DEPLOY_TEST_MODE" != "1" ]; then', 'if true; then')
prefix = prefix.replace(' && [ "$DEPLOY_TEST_MODE" != 1 ]', '')
(work / 'entry.sh').write_text(prefix + '\n_clean_release_build_cache_after_staging\n'
                               'echo stage >>"$TRACE"\necho restart >>"$TRACE"\n')
PY
mkdir -p "$WORK/repo/scripts"
cat >"$WORK/repo/scripts/build_token.py" <<'PY'
import os
from pathlib import Path
import subprocess
import sys

from release_source import record, write

if sys.argv[1] == '--validate-deploy-lock-timeout':
    print(sys.argv[2])
    sys.exit(0)
if sys.argv[1] == '--release-source':
    Path(sys.argv[2]).parent.mkdir(parents=True, exist_ok=True)
rc = subprocess.call(sys.argv[sys.argv.index('--') + 1:])
if rc == 0 and sys.argv[1] == '--release-source':
    binary, metadata, target = sys.argv[2:5]
    write(Path(metadata), record(Path(binary), os.environ['EXPECTED_SHA'], target))
sys.exit(rc)
PY
cp "$ROOT/scripts/release_source.py" "$WORK/repo/scripts/"
cp "$ROOT/scripts/resolve-config-path.sh" "$WORK/repo/scripts/"
cp "$ROOT/scripts/refresh_release_launchd_plist.py" "$WORK/repo/scripts/"
mkdir -p "$WORK/repo/scripts/lib" "$WORK/repo/skills" "$WORK/repo/policies" "$WORK/repo/dashboard"
cp "$ROOT/scripts/lib/"*.sh "$WORK/repo/scripts/lib/"
cp "$ROOT/scripts/_deploy_policy_generation.sh" "$WORK/repo/scripts/"
cp "$WORK/entry.sh" "$WORK/repo/scripts/entry.sh"
cat >"$WORK/entry-stubs.sh" <<'STUB'
. "$ROOT/scripts/_defaults.sh"
DEPLOY_RUN_STARTED="$SECONDS"
REL_LAUNCHD_ENV_FILE="$WORK/release-env-fixture"
entry_config="${AGENTDESK_CONFIG:-}"
_self_hosted_release_session() { return 1; }
_acquire_release_deploy_lock() { :; }
_resolve_release_server_port() { echo 1; }
if [ "${FIXTURE_USE_CONFIG_BRIDGE:-0}" != 1 ]; then
    _release_config_path() { printf '%s\n' "${AGENTDESK_CONFIG:-$ADK_REL/fixture-config.yml}"; }
fi
wait_for_release_runtime_boot() { :; }
curl() { return 1; }
_check_repo_remote_freshness() { :; }
_schema_order_guard_before_build() { :; }
setup_sccache_env() { return 1; }
_ensure_dashboard_dependencies() { echo dependencies >>"$TRACE"; }
_resolve_default_release_binary() {
    [ "${AGENTDESK_CONFIG:-}" = "$entry_config" ] || return 1
    [ "$PWD" = "$REPO" ] || return 1
    echo "$WORK/built"
}
_cleanup_owned_pg_tunnel_preflight() { :; }
_rollback_pg_tunnel_migration() { :; }
_preflight_builder_pids() { :; }
_preflight_cpu_count() { echo 4; }
_preflight_loadavg_1min() { echo "$FAKE_LOAD"; }
_preflight_mem_pressure_level() { echo 1; }
_preflight_high_cpu_processes() { :; }
_preflight_deploy_target_pids() { :; }
_deploy_to_all_peers() {
    [ "$#" -eq 0 ] && [ "$DEPLOY_BUILT_SOURCE_SHA" = "$EXPECTED_SHA" ] \
        && [ "$AGENTDESK_DEPLOY_TARGET_SHA" = "$EXPECTED_SHA" ] \
        && [ "$FAKE_RELEASE_DB_ROUTE" = shell ] \
        && [ "$DEPLOY_PEER_BINARY_SOURCE" = "$ADK_REL/bin/agentdesk" ] || return 1
    printf 'peer-leg profile=%s fast=%s\n' "$DEPLOY_BUILD_PROFILE" "$AGENTDESK_DEPLOY_FAST" >>"$TRACE"
}
git() {
    case "$3" in
        rev-parse) echo "$EXPECTED_SHA" ;;
        merge-base|status) return 0 ;;
        *) return 2 ;;
    esac
}
npm() { echo dashboard-build >>"$TRACE"; return 19; }
STUB
export ROOT
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
SOURCE_RECORD=""
. "$WORK/defaults.sh"
. "$SCRIPT_DIR/lib/deploy_peers_only.sh"
setup_sccache_env() { return 1; }
trap '_emit_terminal_deploy_marker "$?"; _cleanup_peer_binary' EXIT
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
REL_BINARY="$PEER_ROOT/bin/agentdesk"
. "$WORK/leader-promote-block"
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
SOURCE_RECORD=""
. "$WORK/defaults.sh"
. "$SCRIPT_DIR/lib/deploy_peers_only.sh"
setup_sccache_env() { return 1; }
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
    unset PEER_RUNTIME_MACOS PEER_RUNTIME_HEAD EXPECTED_PEER_FALLBACK
    unset AGENTDESK_DEPLOY_BINARY AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR
    case "$scenario" in
        success|codesign-verbosity|stages|concurrency|detach|leader-staged|leader-live|unavailable) ;;
        default-success) export EXPECTED_PEER_FALLBACK=0 ;;
        fallback-value-not-one) export CORRUPT=hash EXPECTED_PEER_FALLBACK=true ;;
        no-fallback) export CORRUPT=hash ;;
        no-fallback-old-checkout) export OLD_PEER_CHECKOUT=1 ;;
        no-fallback-unavailable) ;;
        launch-fail) export LAUNCH_FAIL=1 ;;
        peer-platform) export PEER_RUNTIME_MACOS=14.0 ;;
        head-target) export PEER_RUNTIME_HEAD=old-head ;;
        hash|size|execute|runtime|format|source|profile|signature-meta|witness-mismatch) export CORRUPT="$scenario" ;;
        optimized-hash) export CORRUPT=hash PYTHONOPTIMIZE=1 ;;
        arch) export PEER_ARCH=x86_64 ;;
        toolchain) export PEER_RUSTC=1.91.0 ;;
        os) export PEER_MACOS=14.0 ;;
        signature) export PEER_SIGNATURE=bad ;;
        transfer) export TRANSFER_FAIL=1 ;;
        *) fail "unknown case $scenario" ;;
    esac
    case "$scenario" in
        default-success|no-fallback*) unset AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK ;;
        fallback-value-not-one) export AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK=true ;;
        *) export AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK=1 ;;
    esac
    export AGENTDESK_DEPLOY_TEST_MODE=1
    : >"$TRACE"
    cp "$WORK/good-binary" "$WORK/leader/bin/signed"
    REPO="$WORK/repo" ADK_REL="$WORK/leader" SCRIPT_DIR="$WORK/repo/scripts"
    DEPLOY_PEER_BINARY_SOURCE="$ADK_REL/bin/signed"
    python3 "$SCRIPT_DIR/release_source.py" seal "$DEPLOY_PEER_BINARY_SOURCE" \
        "$EXPECTED_SHA" aarch64-apple-darwin
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
            if [[ "$scenario" == *unavailable ]]; then unset DEPLOY_PEER_BINARY_SOURCE; fi
            echo leader-smoke >>"$TRACE"
            _deploy_to_one_peer peer >"$WORK/$scenario.log" 2>&1 || deploy_rc=$?
            ;;
    esac
    if [[ "$scenario" == no-fallback* || "$scenario" = fallback-value-not-one ]]; then
        local reason='verification failed: SHA256'
        case "$scenario" in
            no-fallback-unavailable) reason='artifact unavailable' ;;
            no-fallback-old-checkout) reason='target checkout does not support binary-only peer deploys' ;;
        esac
        if [ -n "${AGENTDESK_TEST_LOG_DIR:-}" ]; then
            cp "$WORK/$scenario.log" "$AGENTDESK_TEST_LOG_DIR/$scenario.log"
            cp "$TRACE" "$AGENTDESK_TEST_LOG_DIR/$scenario.trace"
        fi
        [ "$deploy_rc" = 1 ] && grep -q "DEPLOY_FAILED_PEER: $reason" "$WORK/$scenario.log" \
            && ! grep -Ex 'build|activate' "$TRACE" \
            || fail "$scenario rc=$deploy_rc: $(cat "$WORK/$scenario.log" "$TRACE")"
        echo "PASS $scenario"
        exit 0
    fi
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
    if [[ "$scenario" == success || "$scenario" == default-success || "$scenario" == codesign-verbosity \
        || "$scenario" == stages || "$scenario" == witness-mismatch || "$scenario" == leader-* ]]; then
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
    if [ "$scenario" = witness-mismatch ]; then
        [ ! -e "$PEER_ROOT/bin/agentdesk.source.json" ] || fail "mismatched witness was promoted"
    else
        source_id="$(python3 "$SCRIPT_DIR/release_source.py" inspect \
            "$PEER_ROOT/bin/agentdesk.source.json" "$PEER_ROOT/bin/agentdesk")"
        [ "$source_id" = "$EXPECTED_SHA aarch64-apple-darwin" ] || fail "installed witness missing or mismatched"
    fi
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

write_fixture_plist() {
    python3 - "$HOME" "$WORK" "$1" "$2" "$3" "${4:-$WORK/leader}" <<'PY'
import os
from pathlib import Path
import plistlib
import sys

home, work, label, cwd, config, root = sys.argv[1:]
path = Path(home) / 'Library' / 'LaunchAgents' / f'{label}.plist'
path.parent.mkdir(parents=True, exist_ok=True)
with path.open('wb') as stream:
    plistlib.dump(dict(Label=label, WorkingDirectory=cwd, ProgramArguments=[f'{root}/bin/agentdesk', 'dcserver'],
        EnvironmentVariables=dict(HOME=home, PATH=f'{work}/tools:/usr/bin:/bin', AGENTDESK_ROOT_DIR=root,
            AGENTDESK_CONFIG=config, WORK=work, TRACE=os.environ['TRACE'], FAKE_RELEASE_DB_ROUTE='primary',
            FAKE_PENDING=os.environ['FAKE_PENDING'], FAKE_CHECKSUM_MISMATCHES=os.environ.get('FAKE_CHECKSUM_MISMATCHES', ''),
            FAKE_SIGNATURE=os.environ.get('FAKE_SIGNATURE', 'ok') or 'ok')), stream)
PY
}

run_entry_case() (
    local scenario=$1 rc=0 expected_rc=1 reason='load average' expected_builds=0
    local source="$WORK/leader/bin/agentdesk" deployed="$WORK/leader/runtime/release-source.json"
    local stdout="$WORK/$scenario.log" expected_leg="" expected_db=""
    local -a args=()
    export AGENTDESK_DEPLOY_TEST_MODE=1 AGENTDESK_DEPLOY_NO_DETACH=1
    export AGENTDESK_DEPLOY_SKIP_FRESHNESS=1 AGENTDESK_DEPLOY_MAX_LOADAVG=5
    export AGENTDESK_ROOT_DIR="$WORK/leader" AGENTDESK_REPO_DIR="$WORK/repo"
    export HOME="$WORK/home/$scenario"
    export FAKE_RELEASE_DB_ROUTE=shell
    export AGENTDESK_DEPLOY_PEERS=peer FAKE_LOAD=9 FAKE_PENDING='[]'
    unset AGENTDESK_CONFIG AGENTDESK_DEPLOY_BINARY AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR AGENTDESK_DEPLOY_TARGET_SHA
    unset AGENTDESK_DEPLOY_PEER_INVOCATION AGENTDESK_DEPLOY_ALL_NODES AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK
    unset AGENTDESK_DCSERVER_LABEL AGENTDESK_PLIST_REL
    : >"$TRACE"
    mkdir -p "$HOME" "$WORK/leader/"{dashboard/dist,runtime,config}
    rm -f "$WORK/leader/agentdesk.yaml" "$WORK/leader/config/agentdesk.yaml"
    printf '<html></html>\n' >"$WORK/leader/dashboard/dist/index.html"
    printf '%s\n' "${EXPECTED_SHA//0/1}" >"$WORK/leader/dashboard/dist/.build-commit"
    printf 'cluster: {enabled: true, role: hub}\n' >"$WORK/leader/fixture-config.yml"
    printf '%s\n' 'FAKE_RELEASE_DB_ROUTE=primary' "AGENTDESK_ROOT_DIR=$WORK/unrelated" >"$WORK/release-env-fixture"
    cp "$WORK/good-binary" "$source"
    python3 "$WORK/repo/scripts/release_source.py" seal "$source" "$EXPECTED_SHA" aarch64-apple-darwin
    printf '{"repo_head":"%s","build_profile":"release"}\n' "$EXPECTED_SHA" >"$deployed"
    case "$scenario" in
        release-layout-*)
            export FAKE_LOAD=1 AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP=1 FIXTURE_USE_CONFIG_BRIDGE=1
            printf '[toolchain]\nchannel = "fixture"\n' >"$WORK/repo/rust-toolchain.toml"
            printf '%s\n' "AGENTDESK_CONFIG=$WORK/leader/agentdesk.yaml" "PATH=$WORK/tools:/usr/bin:/bin" \
                "FIXTURE_TOOLCHAIN_ROOT=$WORK/repo" "FIXTURE_SERVICE_CWD=$WORK/leader" >"$WORK/release-env-fixture"
            printf 'cluster: {enabled: true, role: hub}\n' >"$WORK/leader/agentdesk.yaml"
            reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
            case "$scenario" in
                release-layout-conflict)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/config/agentdesk.yaml"
                    ;;
                release-layout-runner)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/agentdesk.yaml"
                    ;;
                release-layout-hub)
                    expected_rc=0; expected_builds=1; reason='Building release binary'
                    ;;
                *) fail "unknown entry case $scenario" ;;
            esac
            ;;
        current-plist-runner|current-plist-hub)
            export FAKE_LOAD=1 AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP=1
            local service_config="$WORK/current-service.yml" role=runner
            reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
            if [ "$scenario" = current-plist-hub ]; then
                role=hub; expected_rc=0; expected_builds=1; reason='Building release binary'
            fi
            printf 'cluster: {enabled: true, role: %s}\n' "$role" >"$service_config"
            write_fixture_plist com.agentdesk.release "$WORK/leader" "$service_config"
            ;;
        high-load-install|high-load-dashboard)
            export AGENTDESK_DEPLOY_PEER_INVOCATION=1 AGENTDESK_DEPLOY_TARGET_SHA="$EXPECTED_SHA"
            local compatibility signature
            compatibility="$(REPO="$WORK/repo" _peer_binary_platform)"
            signature="$(_peer_binary_signature "$source")"
            export AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR
            AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR="$(mktemp -d "$WORK/leader/bin/.peer-transfer.XXXXXX")"
            cp "$source" "$AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR/agentdesk"
            python3 - "$AGENTDESK_DEPLOY_PEER_ARTIFACT_DIR" "$EXPECTED_SHA" "$compatibility" "$signature" <<'PYMANIFEST'
import hashlib
import json
from pathlib import Path
import sys

directory, sha, platform, signature = sys.argv[1:]
root = Path(directory)
binary = (root / "agentdesk").read_bytes()
(root / "manifest.json").write_text(json.dumps(dict(
    format=1, source_sha=sha, platform=platform, signature=signature,
    profile="release", size=len(binary), sha256=hashlib.sha256(binary).hexdigest())))
PYMANIFEST
            if [ "$scenario" = high-load-install ]; then
                expected_rc=0
                reason='verified; using signed release binary'
            else
                rm -f "$WORK/leader/dashboard/dist/.build-commit"
            fi
            ;;
        high-load-source) ;;
        direct-peer)
            export FAKE_LOAD=1
            printf 'cluster:\n  enabled: true\n  role: runner\n' >"$WORK/leader/fixture-config.yml"
            reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
            ;;
        release-env-*|standalone-source)
            export FAKE_LOAD=1 AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP=1
            local selected_config="$WORK/selected-config.yml"
            rm -f "$WORK/leader/fixture-config.yml" "$selected_config"
            printf 'AGENTDESK_CONFIG=%s\n' "$selected_config" >"$WORK/release-env-fixture"
            case "$scenario" in
                release-env-runner)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$selected_config"
                    export AGENTDESK_CONFIG="$WORK/caller-config.yml"
                    printf 'cluster: {enabled: true, role: hub}\n' >"$AGENTDESK_CONFIG"
                    reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
                    ;;
                release-env-caller)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/fixture-config.yml"
                    printf 'cluster: {enabled: false}\n' >"$selected_config"
                    printf '# Release uses its default config.\n' >"$WORK/release-env-fixture"
                    export AGENTDESK_CONFIG="$selected_config"
                    reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
                    ;;
                release-env-relative-runner|release-env-relative-hub)
                    local relative_config=relative-role.yml
                    printf 'AGENTDESK_CONFIG=%s\n' "$relative_config" >"$WORK/release-env-fixture"
                    if [ "$scenario" = release-env-relative-runner ]; then
                        printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/$relative_config"
                        printf 'cluster: {enabled: false}\n' >"$WORK/repo/$relative_config"
                        reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
                    else
                        printf 'cluster: {enabled: true, role: hub}\n' >"$WORK/leader/$relative_config"
                        rm -f "$WORK/repo/$relative_config"
                        expected_rc=0; expected_builds=1; reason='Building release binary'
                    fi
                    ;;
                release-env-hub)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/fixture-config.yml"
                    printf 'cluster: {enabled: true, role: hub}\n' >"$selected_config"
                    expected_rc=0; expected_builds=1; reason='Building release binary'
                    ;;
                release-env-toolchain-hub)
                    export FIXTURE_USE_CONFIG_BRIDGE=1
                    printf '[toolchain]\nchannel = "fixture"\n' >"$WORK/repo/rust-toolchain.toml"
                    printf 'cluster: {enabled: true, role: hub}\n' >"$WORK/leader/toolchain-role.yml"
                    printf '%s\n' 'AGENTDESK_CONFIG=toolchain-role.yml' "PATH=$WORK/tools:/usr/bin:/bin" \
                        "FIXTURE_TOOLCHAIN_ROOT=$WORK/repo" "FIXTURE_SERVICE_CWD=$WORK/leader" >"$WORK/release-env-fixture"
                    expected_rc=0; expected_builds=1; reason='Building release binary'
                    ;;
                release-env-missing)
                    reason='Cannot determine the cluster role'
                    ;;
                standalone-source)
                    rm -f "$WORK/release-env-fixture" "$source" "$source.source.json" "$deployed"
                    expected_rc=0; expected_builds=1; reason='Building release binary'
                    ;;
                *) fail "unknown entry case $scenario" ;;
            esac
            ;;
        custom-plist-*)
            export FAKE_LOAD=1 AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP=1 AGENTDESK_DCSERVER_LABEL=service.fixture
            local service_cwd="$WORK/service-cwd" config="$WORK/service-cwd/plist-role.yml"
            mkdir -p "$service_cwd"
            printf 'cluster: {enabled: false}\n' >"$WORK/leader/fixture-config.yml"
            printf 'cluster: {enabled: true, role: runner}\n' >"$config"
            reason='Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY'
            case "$scenario" in
                custom-plist-explicit-runner) ;;
                custom-plist-relative-runner)
                    config=${config##*/}
                    printf 'cluster: {enabled: false}\n' >"$WORK/leader/$config"
                    ;;
                custom-plist-overlay-runner)
                    printf 'cluster: {enabled: true, role: hub}\n' >"$config"
                    printf 'cluster: {enabled: true, role: runner}\n' >"$service_cwd/overlaid-role.yml"
                    printf 'AGENTDESK_CONFIG=overlaid-role.yml\n' >>"$WORK/release-env-fixture"
                    ;;
                custom-plist-hub)
                    printf 'cluster: {enabled: true, role: hub}\n' >"$config"
                    expected_rc=0; expected_builds=1; reason='Building release binary'
                    ;;
                custom-plist-tmux-runner)
                    printf 'cluster: {enabled: true, role: hub}\n' >"$config"
                    printf 'cluster: {enabled: true, role: hub}\n' >"$service_cwd/next-role.yml"
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/next-role.yml"
                    printf 'AGENTDESK_CONFIG=next-role.yml\n' >>"$WORK/release-env-fixture"
                    ;;
                custom-plist-missing)
                    reason='Cannot verify the existing release plist'
                    ;;
                *) fail "unknown entry case $scenario" ;;
            esac
            if [ "$scenario" != custom-plist-missing ]; then
                write_fixture_plist "$AGENTDESK_DCSERVER_LABEL" "$service_cwd" "$config"
            fi
            ;;
        peers-only*)
            export FAKE_LOAD=1
            args=(--peers-only)
            local service_cwd="$WORK/leader" plist_config="$WORK/leader/fixture-config.yml" plist_root="$WORK/leader"
            case "$scenario" in
                peers-only)
                    expected_rc=0; reason='migrations already applied'
                    expected_leg='peer-leg profile=release fast=0'
                    ;;
                peers-only-fast)
                    expected_rc=0; reason='migrations already applied'
                    expected_leg='peer-leg profile=release-fast fast=1'
                    printf '{"repo_head":"%s","build_profile":"release-fast"}\n' "$EXPECTED_SHA" >"$deployed"
                    ;;
                peers-only-plist-runner)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$plist_config"
                    printf 'cluster: {enabled: true, role: hub}\n' >"$WORK/leader/next-role.yml"
                    printf 'AGENTDESK_CONFIG=next-role.yml\n' >>"$WORK/release-env-fixture"
                    reason='run on the primary with a known cluster role'
                    ;;
                peers-only-tmux-runner)
                    printf 'cluster: {enabled: true, role: runner}\n' >"$WORK/leader/next-role.yml"
                    printf 'AGENTDESK_CONFIG=%s/next-role.yml\n' "$WORK/leader" >>"$WORK/release-env-fixture"
                    reason='run on the primary with a known cluster role'
                    ;;
                peers-only-db-*)
                    local next_db=db-current
                    expected_db=db-pending
                    reason='pending=\[2\]'
                    if [[ "$scenario" == *-current ]]; then
                        expected_db=db-current; next_db=db-pending
                        expected_rc=0; reason='migrations already applied'
                        expected_leg='peer-leg profile=release fast=0'
                    fi
                    service_cwd="$WORK/service-db-cwd"
                    mkdir -p "$service_cwd"
                    printf 'cluster: {enabled: true, role: hub}\n# fixture DB: %s\n' "$expected_db" >"$service_cwd/service-db.yml"
                    printf 'cluster: {enabled: true, role: hub}\n# fixture DB: %s\n' "$next_db" >"$WORK/leader/next-db.yml"
                    printf 'AGENTDESK_CONFIG=%s/next-db.yml\n' "$WORK/leader" >>"$WORK/release-env-fixture"
                    plist_config="$service_cwd/service-db.yml"
                    [[ "$scenario" != *-relative-* ]] || plist_config=service-db.yml
                    ;;
                peers-only-root-mismatch)
                    plist_root="$WORK/peer"
                    reason='service root does not match'
                    ;;
                peers-only-wrong-head)
                    printf '{"repo_head":"%s"}\n' "${EXPECTED_SHA//0/1}" >"$deployed"
                    reason='deployed head'
                    ;;
                peers-only-migration) export FAKE_PENDING='[2]'; reason='pending=\[2\]' ;;
                peers-only-checksum) export FAKE_CHECKSUM_MISMATCHES='[{}]'; reason='checksum_mismatches=1' ;;
                peers-only-bad-signature) export FAKE_SIGNATURE=bad; reason='signature verification failed' ;;
                peers-only-no-source)
                    rm -f "$source.source.json"
                    reason='source or digest does not match'
                    ;;
                *) fail "unknown entry case $scenario" ;;
            esac
            write_fixture_plist com.agentdesk.release "$service_cwd" "$plist_config" "$plist_root"
            ;;
        *) fail "unknown entry case $scenario" ;;
    esac
    (cd "$WORK/repo" && bash scripts/entry.sh "${args[@]}") >"$stdout" 2>&1 || rc=$?
    if [ -n "${AGENTDESK_TEST_LOG_DIR:-}" ]; then
        mkdir -p "$AGENTDESK_TEST_LOG_DIR"
        cp "$stdout" "$AGENTDESK_TEST_LOG_DIR/$scenario.log"
        cp "$TRACE" "$AGENTDESK_TEST_LOG_DIR/$scenario.trace"
    fi
    [ "$rc" = "$expected_rc" ] && grep -q "$reason" "$stdout" \
        || fail "$scenario rc=$rc expected=$expected_rc: $(cat "$stdout" "$TRACE")"
    case "$scenario" in
        high-load-dashboard)
            grep -q 'verified; using signed release binary' "$stdout" || fail "$scenario checked load before binary selection"
            ! grep -qx dependencies "$TRACE" || fail "$scenario reached dependency installation"
            ;;
        high-load-install)
            grep -qx restart "$TRACE" || fail "$scenario did not reach activation"
            ! grep -qx dependencies "$TRACE" || fail "$scenario installed dashboard dependencies"
            ;;
        release-env-hub|release-env-relative-hub|release-env-toolchain-hub|standalone-source|custom-plist-hub|release-layout-hub|current-plist-hub)
            grep -qx restart "$TRACE" || fail "$scenario did not reach activation"
            ;;
        *) ! grep -Ex 'stage|restart' "$TRACE" || fail "$scenario reached local staging or restart" ;;
    esac
    [ "$(grep -cx build "$TRACE" || true)" = "$expected_builds" ] || fail "$scenario expected $expected_builds source builds"
    ! grep -qx dashboard-build "$TRACE" || fail "$scenario unexpectedly built the dashboard"
    if [ -n "$expected_leg" ]; then
        [ "$(grep '^peer-leg ' "$TRACE")" = "$expected_leg" ] || fail "$scenario did not run exactly one peer stage with the deployed profile"
    else
        ! grep -q '^peer-leg ' "$TRACE" || fail "$scenario unexpectedly dispatched peers"
    fi
    if [ -n "$expected_db" ]; then
        [ "$(grep '^doctor-route ' "$TRACE")" = "doctor-route $expected_db" ] || fail "$scenario used a different database route"
    fi
    echo "PASS $scenario"
)

run_scenario() {
    case "$1" in
        high-load-*|direct-peer|release-env-*|release-layout-*|standalone-source|custom-plist-*|current-plist-*|peers-only*) run_entry_case "$1" ;;
        *) run_case "$1" ;;
    esac
}

if [ "$#" -gt 0 ]; then
    run_scenario "$1"
else
    count=0
    for scenario in success codesign-verbosity hash arch transfer stages size signature execute runtime toolchain os format source profile signature-meta optimized-hash concurrency detach leader-staged leader-live witness-mismatch unavailable launch-fail peer-platform head-target \
        high-load-install high-load-source high-load-dashboard no-fallback no-fallback-unavailable no-fallback-old-checkout \
        default-success fallback-value-not-one direct-peer release-env-runner release-env-caller release-env-relative-runner \
        release-env-relative-hub release-env-hub release-env-toolchain-hub release-env-missing standalone-source \
        release-layout-conflict release-layout-runner release-layout-hub current-plist-runner current-plist-hub \
        custom-plist-explicit-runner custom-plist-relative-runner custom-plist-overlay-runner custom-plist-hub custom-plist-tmux-runner custom-plist-missing \
        peers-only peers-only-fast peers-only-wrong-head peers-only-migration peers-only-checksum \
        peers-only-bad-signature peers-only-no-source peers-only-plist-runner peers-only-tmux-runner peers-only-root-mismatch \
        peers-only-db-pending peers-only-db-current peers-only-db-relative-pending peers-only-db-relative-current; do
        run_scenario "$scenario"
        count=$((count + 1))
    done
    echo "peer transfer: $count passed, 0 failed, 0 skipped"
fi
