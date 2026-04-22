#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/archive-config-backups.sh [--root PATH] [--date YYYY-MM-DD] [--dry-run]

Moves top-level legacy config snapshots from:
  <root>/config/*.pre-*
  <root>/config/*.bak
  <root>/config/*.migrated

into:
  <root>/config/.backups/<date>/
EOF
}

ROOT="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
ARCHIVE_DATE="$(date '+%Y-%m-%d')"
DRY_RUN=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --root)
      [ "$#" -ge 2 ] || {
        echo "missing value for --root" >&2
        exit 64
      }
      ROOT="$2"
      shift 2
      ;;
    --date)
      [ "$#" -ge 2 ] || {
        echo "missing value for --date" >&2
        exit 64
      }
      ARCHIVE_DATE="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

CONFIG_DIR="${ROOT%/}/config"
ARCHIVE_DIR="${CONFIG_DIR}/.backups/${ARCHIVE_DATE}"

[ -d "$CONFIG_DIR" ] || {
  echo "config directory not found: $CONFIG_DIR" >&2
  exit 1
}

matches=()
while IFS= read -r path; do
  [ -n "$path" ] && matches+=("$path")
done < <(
  find "$CONFIG_DIR" -maxdepth 1 -type f \
    \( -name '*.pre-*' -o -name '*.bak' -o -name '*.migrated' \) \
    | LC_ALL=C sort
)

if [ "${#matches[@]}" -eq 0 ]; then
  echo "No legacy config snapshots found under $CONFIG_DIR"
  exit 0
fi

if [ "$DRY_RUN" -eq 0 ]; then
  mkdir -p "$ARCHIVE_DIR"
fi

for source_path in "${matches[@]}"; do
  target_path="$ARCHIVE_DIR/$(basename "$source_path")"
  if [ -e "$target_path" ]; then
    echo "archive target already exists: $target_path" >&2
    exit 1
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'would move %s -> %s\n' "$source_path" "$target_path"
  else
    mv "$source_path" "$target_path"
    printf 'moved %s -> %s\n' "$source_path" "$target_path"
  fi
done

if [ "$DRY_RUN" -eq 0 ]; then
  printf 'Archived %s file(s) into %s\n' "${#matches[@]}" "$ARCHIVE_DIR"
fi
