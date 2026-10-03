#!/usr/bin/env bash
# Prints free -m and df -h for /, /mnt and the cargo target dir so a runner kill
# can be attributed to memory or disk. Always exits 0; output goes to stdout.
set -uo pipefail

label="${1:-snapshot}"
target="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target" in /*) ;; *) target="$PWD/$target" ;; esac

# df needs an existing path; a target or /mnt that is not there yet is
# measured on the nearest existing parent, which is the volume it will land on.
nearest_existing() {
  local path="$1"
  while [ ! -e "$path" ] && [ "$path" != "/" ]; do
    path="$(dirname "$path")"
  done
  printf '%s\n' "$path"
}

mnt="$(nearest_existing /mnt)"
target_volume="$(nearest_existing "$target")"
echo "resource-snapshot ${label}: free -m"
free -m || true
echo "resource-snapshot ${label}: df -h / /mnt(${mnt}) target(${target} -> ${target_volume})"
df -h / "$mnt" "$target_volume" || true
exit 0
