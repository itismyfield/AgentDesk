#!/usr/bin/env bash
# H2 tmux-boundary measurement: scripts/ci/h2_measure.sh --lane linux|macos [--inert] [args...]
# Pins host triple, rustc flags and clippy availability, then runs h2_measure.py.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

lane="" inert=0 mode="--check"
for arg in "$@"; do
  case "$arg" in
    --lane=*) lane="${arg#--lane=}" ;;
    linux | macos) [ "${prev:-}" = --lane ] && lane="$arg" ;;
    --inert) inert=1 ;;
    --regen) mode="" ;;
  esac
  prev="$arg"
done

case "$lane" in
  macos | linux) ;;
  *) echo "h2: --lane linux|macos is required" >&2; exit 2 ;;
esac

# Inert until a baseline is committed: skip the toolchain work entirely.
if [ "$inert" = 1 ] && ! compgen -G 'scripts/ci/h2_baseline_*.toml' >/dev/null; then
  echo "h2: no baseline committed; inert no-op (lane ${lane})"
  exit 0
fi

# Diagnostics must come from the host target with the default flag set.
env_setup="$(python3 scripts/ci/h2_env.py --lane "$lane" --shell)"
eval "$env_setup"

exec "${PYTHON:-python3}" scripts/ci/h2_measure.py ${mode:+"$mode"} "$@"
