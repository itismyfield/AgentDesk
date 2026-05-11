#!/usr/bin/env bash
set -euo pipefail

DRY_RUN=0

usage() {
  cat <<'USAGE'
Usage: scripts/deploy-dev.sh [--dry-run]

Runs the development voice dependency doctor. The dev runtime deployment path
has been retired; use scripts/deploy-release.sh for release deployment.
USAGE
}

for arg in "$@"; do
  case "$arg" in
    --dry-run)
      DRY_RUN=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

missing=0

check_required_command() {
  local name="$1"
  local hint="$2"

  if command -v "$name" >/dev/null 2>&1; then
    echo "ok: found $name at $(command -v "$name")"
    return 0
  fi

  echo "Error: required voice dependency missing: $name. $hint" >&2
  missing=1
}

check_required_command "whisper-cli" "Install whisper.cpp and ensure whisper-cli is on PATH."
check_required_command "ffmpeg" "Install ffmpeg and ensure ffmpeg is on PATH."
check_required_command "edge-tts" "Install edge-tts and ensure edge-tts is on PATH."

if [ "$missing" -ne 0 ]; then
  exit 1
fi

if [ "$DRY_RUN" = "1" ]; then
  echo "ok: deploy-dev dry run complete"
  exit 0
fi

echo "Error: scripts/deploy-dev.sh only runs the voice dependency doctor." >&2
echo "Use scripts/deploy-release.sh for deployment." >&2
exit 1
