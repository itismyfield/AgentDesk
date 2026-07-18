#!/usr/bin/env bash
set -euo pipefail

# AgentDesk dcserver restart script
# Supports both dev and release environments

ENV="${1:-release}"
case "$ENV" in
  dev)
    LABEL="com.agentdesk.dev"
    PORT="${AGENTDESK_DEV_PORT:-8799}"
    RUNTIME_ROOT="$HOME/.adk/dev"
    ;;
  release)
    LABEL="com.agentdesk.release"
    PORT="${AGENTDESK_REL_PORT:-8791}"
    RUNTIME_ROOT="$HOME/.adk/release"
    ;;
  preview)
    LABEL="com.itismyfield.remotecc.dcserver.preview"
    ;;
  *)       echo "Usage: $0 [dev|release|preview]" >&2; exit 1 ;;
esac

WAIT_SECONDS="${AGENTDESK_RESTART_WAIT:-20}"
LIVE_TURN_WAIT_SECONDS="${AGENTDESK_RESTART_LIVE_TURN_WAIT:-120}"
REPO_DIR="${AGENTDESK_REPO_DIR:-$HOME/.adk/release/workspaces/agentdesk}"
DEFAULTS_SH="$REPO_DIR/scripts/_defaults.sh"
MARKER_ARMED=0

cleanup_restart_drain() {
  if [[ "${MARKER_ARMED}" == "1" && -n "${RUNTIME_ROOT:-}" ]] && declare -F clear_restart_drain_mode >/dev/null 2>&1; then
    clear_restart_drain_mode "$RUNTIME_ROOT"
  fi
}

trap cleanup_restart_drain EXIT

if [[ -n "${TMUX:-}" && -n "${TMUX_PANE:-}" ]]; then
  current_tmux_session="$(tmux display-message -p -t "$TMUX_PANE" '#S' 2>/dev/null || true)"
  if [[ -n "$current_tmux_session" && "$current_tmux_session" == AgentDesk-* ]]; then
    echo "REFUSE: do not restart dcserver from an AgentDesk work session ($current_tmux_session)" >&2
    exit 2
  fi
fi

if [[ "$ENV" != "preview" ]]; then
  if [[ ! -f "$DEFAULTS_SH" ]]; then
    echo "REFUSE: safe restart helper not found: $DEFAULTS_SH" >&2
    exit 1
  fi
  # shellcheck source=/dev/null
  . "$DEFAULTS_SH"

  # #1447: preflight assertion guards against the silent-fail mode where
  # _defaults.sh is sourced successfully but is missing the drain helpers
  # (older mirror, partial cherry-pick, etc). Without this, `if ! helper`
  # against an undefined function triggered `command not found` whose exit
  # propagation was inconsistent depending on the caller layout.
  if declare -F assert_restart_helpers_loaded >/dev/null 2>&1; then
    if ! assert_restart_helpers_loaded; then
      exit 1
    fi
  else
    echo "REFUSE: _defaults.sh loaded but lacks assert_restart_helpers_loaded — refusing restart (#1447)" >&2
    exit 1
  fi

  # Every provider role participates in the same marker handshake. Health is
  # drain evidence only after restart_pending has fenced new admissions; an
  # idle snapshot by itself cannot authorize bootout because a standby intake
  # worker may claim a full turn immediately after that snapshot.
  if ! request_restart_drain_mode_or_fail "$ENV" "$LABEL" "$PORT" "$RUNTIME_ROOT" "agentdesk-restart-skill"; then
    exit 1
  fi
  MARKER_ARMED=1

  if ! wait_for_live_turns_to_drain_or_fail "$ENV" "$LABEL" "$PORT" "$LIVE_TURN_WAIT_SECONDS" 2; then
    exit 1
  fi
fi

echo "Restarting $LABEL..."
if [[ "$ENV" == "preview" ]]; then
  launchctl kickstart -k "gui/$(id -u)/${LABEL}" 2>/dev/null || {
    echo "kickstart failed, trying bootout + bootstrap..."
    launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
    sleep 1
    launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/${LABEL}.plist"
  }
else
  launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
  sleep 1
  clear_restart_drain_mode "$RUNTIME_ROOT"
  MARKER_ARMED=0

  if ! launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/${LABEL}.plist"; then
    echo "AGENTDESK_RESTART_BOOTSTRAP_FAILED env=${ENV} label=${LABEL}" >&2
    exit 1
  fi
  _kickstart_launchd_job_if_needed "$LABEL" >/dev/null 2>&1 || true
fi

deadline=$(( $(date +%s) + WAIT_SECONDS ))
launchd_target="gui/$(id -u)/${LABEL}"

while (( $(date +%s) < deadline )); do
  if [[ "$ENV" == "preview" ]]; then
    if launchctl print "$launchd_target" 2>/dev/null | grep -q "state = running"; then
      echo "AGENTDESK_RESTART_OK env=${ENV} label=${LABEL}"
      exit 0
    fi
  elif wait_for_http_service_health "$LABEL" "$PORT" 1 1 0 1 >/dev/null 2>&1; then
    echo "AGENTDESK_RESTART_OK env=${ENV} label=${LABEL} port=${PORT}"
    exit 0
  fi
  sleep 1
done

echo "AGENTDESK_RESTART_TIMEOUT label=${LABEL}" >&2
launchctl print "$launchd_target" 2>/dev/null | sed -n '1,20p' >&2 || true
exit 1
