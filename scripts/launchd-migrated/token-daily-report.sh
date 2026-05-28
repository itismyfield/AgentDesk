#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
TOKEN_MANAGER_CHANNEL="1481478222439907560"
OUTPUT_FILE="$(mktemp "${TMPDIR:-/tmp}/token-daily-report-output.XXXXXX")"

# Step 1: Python으로 원시 데이터 수집
RAW_DATA="$(/usr/bin/python3 "$SCRIPT_DIR/token-daily-report.py" --raw-json)"

PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE" "$OUTPUT_FILE"' EXIT

cat >"$PROMPT_FILE" <<PROMPT
당신은 Token Manager(토큰 매니저)입니다.
아래 원시 데이터를 분석하여 Discord용 한국어 일일 토큰 리포트를 작성하세요.

현재 시각: $NOW_KST

## 원시 데이터
$RAW_DATA

## 리포트 작성 규칙
1. **Rate Limit 현황**: 5h/7d/Sonnet 사용률, 리셋까지 남은 시간
2. **어제 토큰 사용량**: 총합, 입력/출력 비율, Top 5 프로젝트/세션
3. **에이전트별 누적 토큰**: 리셋 이후 누적, 점유율(%)
4. **특이사항 분석**:
   - rate limit 80%+ 경고
   - 특정 에이전트가 70%+ 독식하면 원인 분석
   - 일일 토큰이 평소 대비 급증했으면 원인 추정
   - 출력/입력 비율이 비정상이면 패턴 분석
   - 비효율적 사용 패턴 식별 (반복 컨텍스트 로드, 불필요 대량 생성 등)
5. **효율화 권고**: [대상] → [문제] → [제안] → [예상 절감] 형식
6. 비용 데이터는 USD 기준, 토큰 수는 K/M 단위
7. Discord 마크다운 사용 (테이블 금지, 볼드/리스트만)
8. 전체 1500자 이내로 간결하게

특이사항이 없으면 "정상 범위"로 짧게 마무리.
데이터가 부족하면 있는 것만으로 작성.

CRITICAL OUTPUT INSTRUCTION:
- Write the final message to the file: $OUTPUT_FILE
- Use a single Bash command: cat > $OUTPUT_FILE << 'MSGEOF' ... MSGEOF
- The file must contain ONLY the message text, nothing else.
- If the result is NO_REPLY, write exactly NO_REPLY to the file.
- Your stdout text does not matter. Only the file content will be used.
PROMPT

set +e
cd "$AGENTDESK_OPERATOR_WORKDIR" && claude -p \
  --model sonnet \
  --permission-mode bypassPermissions \
  --output-format text \
  < "$PROMPT_FILE" > /dev/null 2>/dev/null
STATUS=$?
set -e

if [[ "$STATUS" -ne 0 ]]; then
  echo "claude exec failed: exit=$STATUS" >&2
  exit "$STATUS"
fi

if [[ ! -f "$OUTPUT_FILE" ]]; then
  echo "claude did not write output file" >&2
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

# Split and send via PCD API
CHUNK_DIR="$(mktemp -d)"
trap 'rm -rf "$CHUNK_DIR" "$PROMPT_FILE" "$OUTPUT_FILE"' EXIT

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

ADK_API="http://127.0.0.1:${ADK_API_PORT:-8791}/api/discord/send"
SEND_FAILED=0

for f in "$CHUNK_DIR"/*.txt; do
  [[ -f "$f" ]] || continue
  PAYLOAD="$(python3 -c "
import json, sys
msg = open(sys.argv[1], encoding='utf-8').read().strip()
print(json.dumps({'target': 'channel:${TOKEN_MANAGER_CHANNEL}', 'content': msg, 'source': 'token-manager', 'bot': 'notify'}))
" "$f")"
  RESPONSE="$(curl -sS -X POST "$ADK_API" \
    -H "Content-Type: application/json" \
    -d "$PAYLOAD" \
    -w $'\n%{http_code}')"
  HTTP_CODE="${RESPONSE##*$'\n'}"
  RESPONSE_BODY="${RESPONSE%$'\n'*}"
  if [[ "$HTTP_CODE" != "200" ]] || ! printf '%s' "$RESPONSE_BODY" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ok') else 1)" 2>/dev/null; then
    echo "Failed to send chunk $(basename "$f"): HTTP $HTTP_CODE body=${RESPONSE_BODY}" >&2
    SEND_FAILED=1
  fi
  sleep 1
done

if [[ "$SEND_FAILED" -ne 0 ]]; then
  exit 1
fi

echo "Token daily report sent via Sonnet"
