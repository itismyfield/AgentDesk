#!/usr/bin/env bash
# Adds a swapfile on Linux runners so a lib test build that outgrows RAM pages
# instead of being OOM-killed. Every problem is a warning; the job never fails here.
set -uo pipefail

size_gb="${ENSURE_SWAP_SIZE_GB:-16}"
swap_path="${ENSURE_SWAP_PATH:-/mnt/agentdesk-swapfile}"

warn() {
  printf '::warning title=ensure-swap::%s\n' "$*"
}

snapshot="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)/scripts/ci/resource-snapshot.sh"

# Memory and disk state before the build; mem-measure prints the same after it.
report() {
  bash "$snapshot" before-build || true
  swapon --show || true
}

finish() {
  report
  exit 0
}

if [ "$(uname -s)" != "Linux" ]; then
  echo "ensure-swap: skipped on $(uname -s)"
  exit 0
fi
if ! [[ "$size_gb" =~ ^[1-9][0-9]*$ ]]; then
  warn "size-gb must be a positive integer, got '$size_gb'"
  finish
fi

want_mib=$((size_gb * 1024))
have_mib="$(free -m | awk '/^Swap:/ { print $2 }')"
[[ "$have_mib" =~ ^[0-9]+$ ]] || have_mib=0
if [ "$have_mib" -ge "$want_mib" ]; then
  echo "ensure-swap: active swap ${have_mib} MiB already reaches ${want_mib} MiB"
  finish
fi
if [ -e "$swap_path" ]; then
  warn "$swap_path already exists; leaving it alone"
  finish
fi

# Keep 2 GiB free on the volume for the build's own scratch files.
avail_mib="$(df -Pm "$(dirname "$swap_path")" 2>/dev/null | awk 'NR == 2 { print $4 }')"
if ! [[ "$avail_mib" =~ ^[0-9]+$ ]] || [ "$avail_mib" -lt $((want_mib + 2048)) ]; then
  warn "not enough free space for ${size_gb}G at $swap_path (available: ${avail_mib:-unknown} MiB)"
  finish
fi

if sudo -n fallocate -l "${size_gb}G" "$swap_path" &&
  sudo -n chmod 600 "$swap_path" &&
  sudo -n mkswap "$swap_path" >/dev/null &&
  sudo -n swapon "$swap_path"; then
  echo "ensure-swap: added ${size_gb}G swap at $swap_path (previous swap ${have_mib} MiB)"
else
  warn "could not enable ${size_gb}G swap at $swap_path; continuing without it"
  # The path did not exist before this script, so the partial file is ours.
  [ -e "$swap_path" ] && sudo -n rm -f "$swap_path"
fi
finish
