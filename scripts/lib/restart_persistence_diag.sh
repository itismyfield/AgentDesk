#!/usr/bin/env bash

_restart_persistence_artifact_diag() {
  local root="$1" file="$2" expected_nonce="$3" observed="" rc=0
  printf '  last proof observation: file=%s ' "$file"
  if [ ! -e "$root/$file" ]; then
    printf 'state=missing\n'
  elif [ ! -f "$root/$file" ]; then
    printf 'state=not-regular-file\n'
  else
    observed="$(grep -m1 '^nonce=' "$root/$file" 2>/dev/null)" || rc=$?
    if [ "$rc" -gt 1 ]; then
      printf 'state=unreadable\n'
    elif [ "$rc" -eq 1 ]; then
      printf 'state=malformed reason=missing-nonce\n'
    elif ! _restart_nonce_is_path_safe "${observed#nonce=}"; then
      printf 'state=malformed reason=invalid-nonce\n'
    elif [ "${observed#nonce=}" = "$expected_nonce" ]; then
      printf 'state=nonce-observed-after-failed-check\n'
    else
      # Only a bounded, path-safe nonce may leave the marker; never dump its body.
      printf 'state=nonce-mismatch observed_nonce=%s\n' "${observed#nonce=}"
    fi
  fi
}

_restart_persistence_failure_snapshot() {
  local root="$1" nonce="$2"
  _restart_nonce_is_path_safe "$nonce" || return 1
  _restart_persistence_artifact_diag "$root" "restart_persisted.$nonce" "$nonce"
  if [ "${AGENTDESK_RESTART_LEGACY_INDEX_COMPAT:-1}" = 0 ]; then
    printf '  last proof observation: file=restart_persisted state=legacy-index-disabled\n'
  else
    _restart_persistence_artifact_diag "$root" restart_persisted "$nonce"
  fi
}
