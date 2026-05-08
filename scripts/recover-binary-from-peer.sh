#!/usr/bin/env bash
# Emergency binary recovery — copy `~/.adk/release/bin/agentdesk` from a peer
# node when this node's binary is broken (crash loop, missing migration, etc.)
# and a fresh `deploy-release.sh` build is too slow to reach.
#
# What this script handles for you:
# - SSH into the peer and stage its binary into a local temp file
# - Verify the staged binary is executable + Mach-O for arm64
# - Stop the local dcserver via launchctl
# - chflags nouchg the existing binary (the immutable lock blocks plain cp/mv;
#   2026-05-09 incident required manual chflags)
# - mv the staged binary into place
# - chflags uchg + chmod +x to restore the immutable + executable invariants
# - Start dcserver via launchctl
# - Poll /api/health until ready or timeout
#
# Usage:
#   scripts/recover-binary-from-peer.sh <peer-ssh-host>
#
# Example:
#   scripts/recover-binary-from-peer.sh mac-book
#
# This script intentionally does NOT codesign — it assumes the peer's binary
# was already signed by `deploy-release.sh sign_binary_with_fallback`. If the
# peer binary is unsigned the local launchd will SIGKILL it and you should run
# `deploy-release.sh` for a full sign+deploy instead.

set -euo pipefail

if [ $# -lt 1 ]; then
  cat >&2 <<EOF
usage: $0 <peer-ssh-host>

example:
  $0 mac-book   # SSH into mac-book and copy its binary onto this host
EOF
  exit 2
fi

PEER="$1"
LOCAL_ROOT="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
LOCAL_BIN="$LOCAL_ROOT/bin/agentdesk"
LOCAL_PID_FILE="$LOCAL_ROOT/runtime/dcserver.pid"
LOCAL_HEALTH_URL="http://127.0.0.1:8791/api/health"
PLIST_REL="com.agentdesk.release"

STAGED="$(mktemp "$LOCAL_ROOT/bin/agentdesk.recover.XXXXXX")"
trap 'rm -f "$STAGED"' EXIT

echo "▸ Resolving peer binary path on $PEER..."
PEER_BIN=$(ssh "$PEER" 'echo "${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}/bin/agentdesk"')
echo "  peer binary: $PEER:$PEER_BIN"

echo "▸ Copying peer binary into local staging..."
scp -q "$PEER:$PEER_BIN" "$STAGED"

if ! file "$STAGED" | grep -q 'Mach-O .* arm64'; then
  echo "✗ Staged binary is not Mach-O arm64 — refusing to swap." >&2
  file "$STAGED" >&2
  exit 1
fi
chmod +x "$STAGED"
echo "  staged: $STAGED ($(stat -f '%z' "$STAGED") bytes)"

OLD_PID=""
if [ -f "$LOCAL_PID_FILE" ]; then
  OLD_PID=$(cat "$LOCAL_PID_FILE" 2>/dev/null || true)
fi

echo "▸ Stopping local dcserver..."
launchctl bootout "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
  echo "  waiting for PID $OLD_PID to exit..."
  WAIT=0
  while kill -0 "$OLD_PID" 2>/dev/null && [ "$WAIT" -lt 15 ]; do
    sleep 1
    WAIT=$((WAIT + 1))
  done
  if kill -0 "$OLD_PID" 2>/dev/null; then
    echo "  ⚠ PID $OLD_PID did not exit after 15s — sending SIGKILL"
    kill -9 "$OLD_PID" 2>/dev/null || true
    sleep 1
  fi
fi

echo "▸ Promoting staged binary..."
chflags nouchg "$LOCAL_BIN" 2>/dev/null || true
mv -f "$STAGED" "$LOCAL_BIN"
trap - EXIT
chmod +x "$LOCAL_BIN"
chflags uchg "$LOCAL_BIN"

echo "▸ Starting local dcserver..."
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>&1 | head -3 || true

echo "▸ Waiting for $LOCAL_HEALTH_URL..."
for i in $(seq 1 60); do
  if curl -fs "$LOCAL_HEALTH_URL" >/dev/null 2>&1; then
    echo "✓ Release is healthy on :8791 (attempt $i)"
    echo "═══ Recovery Complete ═══"
    exit 0
  fi
  sleep 1
done

echo "✗ Local release did not become healthy in 60s — check ~/.adk/release/logs/dcserver.stderr.log" >&2
exit 1
