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
printf '{}\n' >"$REPO/defaults.json"
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
    case "${scenario:-}" in
        head-moved|standalone-head-moved) git -C "$REPO" checkout -q "$NEW" ;;
    esac
    mkdir -p "$REPO/dashboard/dist"
    rm -f "$REPO/dashboard/dist/.build-commit"
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
        target-dirty)
            recorded="$NEW"
            printf 'modified\n' >>"$REPO/dashboard/source"
            ;;
        missing-index)
            recorded="$NEW"
            rm "$ADK_REL/dashboard/dist/index.html"
            ;;
        head-moved) recorded="" ;;
        non-git)
            recorded=""
            REPO="$WORK/non-git"
            mkdir -p "$REPO/dashboard"
            ;;
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
        expected="$TARGET"
        [ "$scenario" != target-dirty ] || expected="$TARGET+dirty"
        if [ "$scenario" = non-git ]; then
            [ ! -e "$ADK_REL/dashboard/dist/.build-commit" ] || fail 'non-git recorded an unproven commit'
            [ "$(grep -Fxc '⚠ dashboard build commit not recorded' "$ADK_REL/build.log")" = 1 ] || fail 'non-git warning missing or repeated'
        else
            [ "$(cat "$ADK_REL/dashboard/dist/.build-commit")" = "$expected" ] || fail "$scenario did not record build-start commit"
        fi
    fi
    git -C "$WORK/repo" checkout -q "$TARGET"
    git -C "$WORK/repo" checkout -q -- dashboard/source defaults.json
    echo "PASS release-$scenario"
)

# shellcheck disable=SC1090
. "$BUILD_LIB"
run_standalone() (
    scenario="$1"
    if [ "$scenario" = standalone-non-git ]; then
        REPO="$WORK/standalone-non-git-repo"
        mkdir -p "$REPO/dashboard"
    fi
    # shellcheck disable=SC2034 # Consumed by the extracted standalone block.
    DASHBOARD_SRC="$REPO/dashboard"
    export AGENTDESK_ROOT_DIR="$WORK/$scenario-root"
    TRACE="$WORK/$scenario-trace"
    # shellcheck disable=SC1091
    . "$WORK/standalone" >"$WORK/$scenario.log"
    [ "$(cat "$TRACE")" = build ] || fail "$scenario did not build exactly once"
    [ "$(cat "$AGENTDESK_ROOT_DIR/dashboard/dist/index.html")" = rebuilt ] || fail "$scenario did not deploy dist"
    if [ "$scenario" = standalone-non-git ]; then
        [ ! -e "$AGENTDESK_ROOT_DIR/dashboard/dist/.build-commit" ] || fail 'standalone non-git recorded an unproven commit'
        [ "$(grep -Fxc '⚠ dashboard build commit not recorded' "$WORK/$scenario.log")" = 1 ] || fail 'standalone non-git warning missing or repeated'
    else
        [ "$(cat "$AGENTDESK_ROOT_DIR/dashboard/dist/.build-commit")" = "$TARGET" ] || fail "$scenario did not record build-start commit"
    fi
    git -C "$WORK/repo" checkout -q "$TARGET"
    echo "PASS $scenario"
)
run_dirty_scope() (
    mkdir -p "$REPO/dashboard/dist"
    printf 'modified\n' >>"$REPO/dashboard/source"
    _dashboard_build_commit write "$(_dashboard_build_commit capture "$REPO")" "$REPO/dashboard/dist"
    [ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET+dirty" ] || fail 'tracked dashboard change was not dirty'
    git -C "$REPO" checkout -q -- dashboard/source
    printf 'untracked\n' >"$REPO/dashboard/untracked"
    _dashboard_build_commit write "$(_dashboard_build_commit capture "$REPO")" "$REPO/dashboard/dist"
    [ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET+dirty" ] || fail 'untracked dashboard change was not dirty'
    mv "$REPO/dashboard/untracked" "$REPO/outside-dashboard"
    _dashboard_build_commit write "$(_dashboard_build_commit capture "$REPO")" "$REPO/dashboard/dist"
    [ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET" ] || fail 'outside change marked dashboard dirty'
    printf 'modified\n' >>"$REPO/defaults.json"
    _dashboard_build_commit write "$(_dashboard_build_commit capture "$REPO")" "$REPO/dashboard/dist"
    [ "$(cat "$REPO/dashboard/dist/.build-commit")" = "$TARGET+dirty" ] || fail 'defaults.json change was not dirty'
    echo 'PASS dashboard-dirty-scope'
)
if [ "$#" -gt 0 ]; then
    case "$1" in
        standalone-*) run_standalone "$1" ;;
        dirty-scope) run_dirty_scope ;;
        *) run_case "$1" ;;
    esac
    exit 0
fi
for scenario in newer same older missing dirty unknown target-dirty head-moved missing-index non-git; do run_case "$scenario"; done
for scenario in standalone-record-staging standalone-head-moved standalone-non-git; do run_standalone "$scenario"; done
run_dirty_scope
echo 'dashboard build commit: 14 passed, 0 failed, 0 skipped'
