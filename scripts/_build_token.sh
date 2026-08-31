#!/usr/bin/env bash

_run_with_build_token_python() {
  local helper_dir helper_path raw_os
  helper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  helper_path="$helper_dir/build_token.py"
  raw_os="$(uname -s | tr '[:upper:]' '[:lower:]')"

  case "$raw_os" in
    msys*|mingw*|cygwin*)
      if command -v py >/dev/null 2>&1 \
        && py -3 -c 'import sys; raise SystemExit(0 if sys.platform == "win32" else 1)' >/dev/null 2>&1; then
        py -3 "$helper_path" -- "$@"
        return
      fi
      if command -v python >/dev/null 2>&1 \
        && python -c 'import sys; raise SystemExit(0 if sys.platform == "win32" else 1)' >/dev/null 2>&1; then
        python "$helper_path" -- "$@"
        return
      fi
      if command -v python3 >/dev/null 2>&1 \
        && python3 -c 'import sys; raise SystemExit(0 if sys.platform == "win32" else 1)' >/dev/null 2>&1; then
        python3 "$helper_path" -- "$@"
        return
      fi
      echo "build-token: native Win32 Python is required; raw Cargo fallback is forbidden" >&2
      return 127
      ;;
    *)
      command -v python3 >/dev/null 2>&1 || {
        echo "build-token: python3 is required" >&2
        return 127
      }
      python3 "$helper_path" -- "$@"
      ;;
  esac
}

_with_build_token() {
  _run_with_build_token_python "$@"
}
