#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

TARGET=""
SOURCE=""
PROMPT_FILE=""
WORKDIR="$AGENTDESK_OPERATOR_WORKDIR"
DRY_RUN=0
# Accepted for compatibility with existing launchd entrypoints.
# shellcheck disable=SC2034
SEARCH=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="$2"
      shift 2
      ;;
    --source)
      SOURCE="$2"
      shift 2
      ;;
    --prompt-file)
      PROMPT_FILE="$2"
      shift 2
      ;;
    --workdir)
      WORKDIR="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --search)
      # shellcheck disable=SC2034
      SEARCH=1
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$SOURCE" || -z "$PROMPT_FILE" ]]; then
  echo "source and prompt-file are required" >&2
  exit 2
fi

if [[ "$DRY_RUN" -eq 0 && -z "$TARGET" ]]; then
  echo "target is required unless dry-run" >&2
  exit 2
fi

# Unique output file for this job run. Keep it unpredictable so overlapping
# routine retries cannot read or clobber each other's result.
OUTPUT_FILE="$(mktemp "${TMPDIR:-/tmp}/claude-job-output-${SOURCE//[:\/]/-}.XXXXXX")"

# Append file-based output instruction to the prompt
AUGMENTED_PROMPT="$(mktemp)"
trap 'rm -f "$AUGMENTED_PROMPT" "$OUTPUT_FILE"' EXIT

cat "$PROMPT_FILE" > "$AUGMENTED_PROMPT"
cat >> "$AUGMENTED_PROMPT" <<EXTRA

CRITICAL OUTPUT INSTRUCTION:
- Write the final message to the file: $OUTPUT_FILE
- Use a single Bash command: cat > $OUTPUT_FILE << 'MSGEOF' ... MSGEOF
- The file must contain ONLY the message text, nothing else.
- If the result is NO_REPLY, write exactly NO_REPLY to the file.
- Your stdout text does not matter. Only the file content will be used.
EXTRA

set +e
cd "$WORKDIR" && claude -p \
  --permission-mode bypassPermissions \
  --output-format text \
  < "$AUGMENTED_PROMPT" > /dev/null 2>/dev/null
STATUS=$?
set -e

if [[ "$STATUS" -ne 0 ]]; then
  echo "claude exec failed: exit=$STATUS source=$SOURCE" >&2
  exit "$STATUS"
fi

if [[ ! -f "$OUTPUT_FILE" ]]; then
  echo "claude did not write output file: source=$SOURCE" >&2
  exit 1
fi

MESSAGE="$(python3 -c "
from pathlib import Path
text = Path('$OUTPUT_FILE').read_text(encoding='utf-8').strip()
print(text, end='')
")"

if [[ -z "$MESSAGE" || "$MESSAGE" == "NO_REPLY" ]]; then
  exit 0
fi

printf '%s\n' "$MESSAGE"

if [[ "$DRY_RUN" -eq 1 ]]; then
  exit 0
fi

CHANNEL_ID="${TARGET#channel:}"

# Use ADK API for message delivery (release port 8791, dev port 8797)
ADK_PORT="${ADK_API_PORT:-8791}"

# Split message into chunks if over Discord 2000 char limit, then send each
CHUNK_DIR="$(mktemp -d)"
trap 'rm -rf "$CHUNK_DIR" "$AUGMENTED_PROMPT" "$OUTPUT_FILE"' EXIT

python3 - "$OUTPUT_FILE" "$CHUNK_DIR" <<'PYCHUNK'
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8").strip()
chunk_dir = Path(sys.argv[2])
max_len = 1900

sections = text.split("\n\n")
chunks, current = [], ""
for sec in sections:
    candidate = (current + "\n\n" + sec).strip() if current else sec
    if len(candidate) <= max_len:
        current = candidate
    else:
        if current:
            chunks.append(current)
        current = sec[:max_len] if len(sec) > max_len else sec
if current:
    chunks.append(current)

for i, chunk in enumerate(chunks):
    (chunk_dir / f"{i:03d}.txt").write_text(chunk, encoding="utf-8")
PYCHUNK

JSON_SOURCE="$(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$SOURCE")"
SEND_FAILED=0
for f in "$CHUNK_DIR"/*.txt; do
  [[ -f "$f" ]] || continue
  # Escape JSON content
  JSON_CONTENT="$(python3 -c "import json,sys; print(json.dumps(sys.stdin.read().strip()))" < "$f")"
  SEND_RESULT="$(curl -s -X POST "http://127.0.0.1:${ADK_PORT}/api/discord/send" \
    -H "Content-Type: application/json" \
    -d "{\"target\":\"channel:${CHANNEL_ID}\",\"content\":${JSON_CONTENT},\"source\":\"${ADK_SEND_SOURCE:-routine-runtime}\",\"bot\":\"${ADK_BOT:-notify}\",\"record_transcript\":true,\"transcript_source_label\":${JSON_SOURCE}}")"
  if ! printf '%s' "$SEND_RESULT" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ok') else 1)" 2>/dev/null; then
    echo "ADK send failed: $SEND_RESULT (source=$SOURCE)" >&2
    SEND_FAILED=1
  fi
  sleep 1
done

if [[ "$SEND_FAILED" -ne 0 ]]; then
  exit 1
fi
