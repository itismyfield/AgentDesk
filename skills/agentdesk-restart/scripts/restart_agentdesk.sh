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

  standby_without_live_turns=false
  if _health_json_has_jq; then
    if ! restart_health_json=$(curl -s --max-time 3 -H "$(_health_origin_header)" \
      "http://${ADK_DEFAULT_LOOPBACK}:${PORT}/api/health/detail" 2>/dev/null); then
      restart_health_json=""
    fi
    if [[ -n "$restart_health_json" ]] && printf '%s\n' "$restart_health_json" | jq -e '
    (.cluster_standby == true)
    and ((.global_active | type) == "number")
    and ((.global_finalizing | type) == "number")
    and (.global_active == 0)
    and (.global_finalizing == 0)
    and (
      [(.providers // [])[] | (.active_turns // 0)] | add // 0
    ) == 0
    and (
      [(.mailboxes // [])[] | select(
        (.has_cancel_token == true)
        or (.inflight_state_present == true)
        or (.relay_health.bridge_inflight_present == true)
        or (.relay_health.mailbox_has_cancel_token == true)
        or (.relay_stall_state == "active_foreground_stream")
      )] | length
    ) == 0
    ' >/dev/null 2>&1; then
      standby_without_live_turns=true
    fi
  fi

  if [[ "$standby_without_live_turns" == "true" ]]; then
    echo "▸ [gate] ${ENV} cluster standby has no active/finalizing/runtime-evidence turns — skipping leader-only restart drain acknowledgement"
  else
    if ! request_restart_drain_mode_or_fail "$ENV" "$LABEL" "$PORT" "$RUNTIME_ROOT" "agentdesk-restart-skill"; then
      exit 1
    fi
    MARKER_ARMED=1
  fi

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
