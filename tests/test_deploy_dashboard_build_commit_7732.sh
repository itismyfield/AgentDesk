#!/usr/bin/env bash
# Exercise dashboard build/staging/swap blocks only, inside a temporary git repo.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
BUILD_LIB="${AGENTDESK_TEST_DASHBOARD_LIB:-$REPO_ROOT/scripts/lib/dashboard_build_commit.sh}"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-dashboard-build.XXXXXX")
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$WORK"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

python3 - "$DEPLOY_SH" "$REPO_ROOT/scripts/deploy-dashboard.sh" "$WORK" <<'PY'
from pathlib import Path
import sys
release, dashboard = (Path(p).read_text() for p in sys.argv[1:3])
out = Path(sys.argv[3])
start = release.index('echo "▸ [timing] phase=release-binary')
end = release.index('# Stage agent prompt files', start)
out.joinpath('build').write_text(release[start:end])
out.joinpath('swap').write_text(release.split('# Atomic swap: old → .old, staged → dist, cleanup', 1)[1].split('rm -rf "$ADK_REL/skills.old"', 1)[0])
out.joinpath('standalone').write_text(dashboard.split('# Build\n', 1)[1])
PY

REPO="$WORK/repo"
mkdir -p "$REPO/dashboard"
git -C "$REPO" init -q
git -C "$REPO" config user.name fixture
git -C "$REPO" config user.email fixture@example.invalid
printf 'dashboard/dist/\n' >"$REPO/.gitignore"
printf 'old\n' >"$REPO/dashboard/source"
git -C "$REPO" add .
git -C "$REPO" commit -qm old
OLD=$(git -C "$REPO" rev-parse HEAD)
printf 'target\n' >"$REPO/dashboard/source"
git -C "$REPO" commit -qam target
TARGET=$(git -C "$REPO" rev-parse HEAD)
printf 'new\n' >"$REPO/dashboard/source"
git -C "$REPO" commit -qam new
NEW=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" checkout -q "$TARGET"

npm() {
    echo build >>"$TRACE"
    mkdir -p "$REPO/dashboard/dist"
    printf 'rebuilt\n' >"$REPO/dashboard/dist/index.html"
}
_resolve_dashboard_source() { printf '%s\n' "$REPO/dashboard/dist"; }

run_case() (
    scenario="$1"
    # shellcheck disable=SC2034 # Consumed by the extracted build block.
    SCRIPT_DIR="$(dirname "$(dirname "$BUILD_LIB")")"
    ADK_REL="$WORK/$scenario"
    TRACE="$ADK_REL/trace"
    mkdir -p "$ADK_REL/dashboard/dist" "$ADK_REL/dashboard/dist.new" "$ADK_REL/dashboard/dist.old"
    : >"$TRACE"
    printf 'deployed\n' >"$ADK_REL/dashboard/dist/index.html"
    printf 'staged\n' >"$ADK_REL/dashboard/dist.new/sentinel"
    printf 'backup\n' >"$ADK_REL/dashboard/dist.old/sentinel"
    case "$scenario" in
        newer) recorded="$NEW" ;;
        same) recorded="$TARGET" ;;
        older) recorded="$OLD" ;;
        missing) recorded="" ;;
        dirty) recorded="$NEW+dirty" ;;
        unknown) recorded=0000000000000000000000000000000000000000 ;;
        *) fail "unknown scenario $scenario" ;;
    esac
    if [ -n "$recorded" ]; then printf '%s\n' "$recorded" >"$ADK_REL/dashboard/dist/.build-commit"; fi
    # shellcheck disable=SC2034 # Consumed by the extracted build block.
    BINARY_PHASE_STARTED="$SECONDS" PEER_BINARY_ACCEPTED=0
    # shellcheck disable=SC1091
    . "$WORK/build" >"$ADK_REL/build.log"
    # shellcheck disable=SC1091
    . "$WORK/swap" >"$ADK_REL/swap.log"
    if [ "$scenario" = newer ]; then
        [ ! -s "$TRACE" ] || fail 'newer dashboard was rebuilt'
        [ "$(cat "$ADK_REL/dashboard/dist/index.html")" = deployed ] || fail 'newer dist was replaced'
        [ "$(cat "$ADK_REL/dashboard/dist/.build-commit")" = "$NEW" ] || fail 'newer record changed'
        [ -f "$ADK_REL/dashboard/dist.new/sentinel" ] || fail 'newer staging changed'
        [ -f "$ADK_REL/dashboard/dist.old/sentinel" ] || fail 'newer backup changed'
        grep -Fx "▸ Dashboard kept: deployed build ${NEW:0:9} is newer than target ${TARGET:0:9}" "$ADK_REL/build.log" >/dev/null || fail 'keep log missing'
        [ ! -s "$ADK_REL/swap.log" ] || fail 'skip attempted to re-stage'
    else
        [ "$(cat "$TRACE")" = build ] || fail "$scenario did not build exactly once"
        [ "$(cat "$ADK_REL/dashboard/dist/index.html")" = rebuilt ] || fail "$scenario did not replace dist"
        [ "$(cat "$ADK_REL/dashboard/dist/.build-commit")" = "$TARGET" ] || fail "$scenario did not record target"
    fi
    echo "PASS release-$scenario"
)

if [ "$#" -gt 0 ]; then run_case "$1"; exit 0; fi
for scenario in newer same older missing dirty unknown; do run_case "$scenario"; done

# shellcheck disable=SC1090
. "$BUILD_LIB"
# shellcheck disable=SC2034 # Consumed by the extracted standalone block.
DASHBOARD_SRC="$REPO/dashboard"
export AGENTDESK_ROOT_DIR="$WORK/standalone-root"
TRACE="$WORK/standalone-trace"
(
    # shellcheck disable=SC1091
    . "$WORK/standalone" >"$WORK/standalone.log"
)
[ "$(cat "$WORK/standalone-root/dashboard/dist/.build-commit")" = "$TARGET" ] || fail 'standalone staging lost build record'
echo 'PASS standalone-record-staging'
printf 'modified\n' >>"$REPO/dashboard/source"
_dashboard_build_commit write "$REPO" "$REPO/dashboard/dist"
[ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET+dirty" ] || fail 'tracked dashboard change was not dirty'
git -C "$REPO" checkout -q -- dashboard/source
printf 'untracked\n' >"$REPO/dashboard/untracked"
_dashboard_build_commit write "$REPO" "$REPO/dashboard/dist"
[ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET+dirty" ] || fail 'untracked dashboard change was not dirty'
mv "$REPO/dashboard/untracked" "$REPO/outside-dashboard"
_dashboard_build_commit write "$REPO" "$REPO/dashboard/dist"
[ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET" ] || fail 'outside change marked dashboard dirty'
echo 'PASS dashboard-dirty-scope'
echo 'dashboard build commit: 8 passed, 0 failed, 0 skipped'
