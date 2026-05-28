#!/usr/bin/env bash
# Shared Python runner resolution for local git hooks and shell checks.

agentdesk_run_python() {
  if [ -n "${PYTHON:-}" ]; then
    if command -v "$PYTHON" >/dev/null 2>&1 || [ -x "$PYTHON" ]; then
      "$PYTHON" "$@"
      return $?
    fi
    printf 'Configured PYTHON runner not found or not executable: %s\n' "$PYTHON" >&2
    return 127
  fi

  if command -v python3 >/dev/null 2>&1; then
    python3 "$@"
    return $?
  fi

  if command -v uv >/dev/null 2>&1; then
    uv run python "$@"
    return $?
  fi

  if command -v py >/dev/null 2>&1; then
    py "$@"
    return $?
  fi

  cat >&2 <<'EOF'
No Python runner found for AgentDesk git hooks.
Install python3, install uv, install the Windows Python launcher, or set PYTHON=/path/to/python.
EOF
  return 127
}
