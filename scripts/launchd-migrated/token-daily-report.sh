#!/bin/bash
# Token Manager Daily Report v2 orchestration.
#
# Design: the FACTUAL report (accounts, resets, usage, drill-down, anomalies) is
# rendered deterministically by token-daily-report.py — NO LLM touches the facts,
# so it cannot invent anomalies (the v1 failure mode). A constrained Sonnet pass
# only appends a short "효율화 권고" section, grounded strictly in the raw JSON.
# If the LLM step fails for any reason, we ship the factual body alone.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PY="/usr/bin/python3"
if [[ -f "$SCRIPT_DIR/_portable-resolver.sh" ]]; then
  # shellcheck source=/dev/null
  source "$SCRIPT_DIR/_portable-resolver.sh"
  agentdesk_source_portable_resolver || true
fi
WORKDIR="${AGENTDESK_OPERATOR_WORKDIR:-$HOME}"
CHANNEL="1481478222439907560"
ADK_API="http://127.0.0.1:${ADK_API_PORT:-8791}/api/discord/send"

BODY_FILE="$(mktemp)"; RAW_FILE="$(mktemp)"; REC_FILE="$(mktemp)"
PROMPT_FILE="$(mktemp)"; OUT_FILE="$(mktemp)"; SENDER_PY="$(mktemp)"
trap 'rm -f "$BODY_FILE" "$RAW_FILE" "$REC_FILE" "$PROMPT_FILE" "$OUT_FILE" "$SENDER_PY"' EXIT

# 1) Deterministic factual body + raw JSON.
"$PY" "$SCRIPT_DIR/token-daily-report.py" --dry-run  > "$BODY_FILE"
"$PY" "$SCRIPT_DIR/token-daily-report.py" --raw-json > "$RAW_FILE"

# 2) Constrained Sonnet pass → ONLY a recommendations section.
{
  echo '당신은 Token Manager입니다. 아래 원시 데이터(RAWJSON)만 근거로 "효율화 권고" 섹션 하나만 작성하세요.'
  echo
  echo '## 절대 규칙 (위반 금지)'
  echo '- RAWJSON의 anomalies / drilldown / account_pressure / resets 에 실제로 존재하는 근거만 사용.'
  echo '- 데이터에 없는 수치·이상·원인을 새로 지어내지 마라. "추정"으로 이상을 만들지 마라.'
  echo '- 캐시 재사용(cache_read)·높은 캐시 히트율은 정상이며 저비용($1.50/MTok)이다. 절대 비효율/문제로 지적하지 마라.'
  echo '- 절감은 순절감 관점으로: cache_create($18.75/MTok)는 cache_read($1.50)보다 12.5배 비싸다. 실질 절감은 "원본 대량 도구출력→cache_create/새입력 증가" 축에서만 제시.'
  echo '- 비용 기여가 작은(<5%) 항목은 권고하지 마라.'
  echo '- 사실 리포트 본문은 이미 확정됐다. 다시 쓰지 말고 권고만 덧붙인다.'
  echo '- 각 권고 형식: [대상] → [문제(근거 인용)] → [제안] → [예상 순절감]'
  echo '- 실행 가능한 권고가 없으면 정확히 "권고 없음"만 출력.'
  echo '- 최대 3개. Discord 마크다운(테이블 금지). 전체 500자 이내.'
  echo
  echo '## RAWJSON'
  cat "$RAW_FILE"
  echo
  echo '## 출력'
  echo '**효율화 권고** 로 시작하는 섹션(또는 "권고 없음").'
  echo "파일로만 출력: cat > $REC_FILE << 'MSGEOF' ... MSGEOF"
} > "$PROMPT_FILE"

set +e
cd "$WORKDIR" && claude -p --model sonnet --permission-mode bypassPermissions \
  --output-format text < "$PROMPT_FILE" >/dev/null 2>/dev/null
set -e

# 3) Compose final = factual body [+ grounded recommendations].
REC=""
if [[ -s "$REC_FILE" ]]; then
  REC="$("$PY" -c "from pathlib import Path;print(Path('$REC_FILE').read_text(encoding='utf-8').strip(),end='')")"
fi
cp "$BODY_FILE" "$OUT_FILE"
if [[ -n "$REC" && "$REC" != "권고 없음" ]]; then
  { echo; echo; printf '%s' "$REC"; } >> "$OUT_FILE"
fi

# 4) Chunk + send via ADK API.
cat > "$SENDER_PY" <<'PYSEND'
import json, sys, urllib.request
api, channel, path = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read().strip()
if not text or text == "NO_REPLY":
    sys.exit(0)
chunks, cur = [], ""
for para in text.split("\n\n"):
    cand = (cur + "\n\n" + para).strip() if cur else para
    if len(cand) <= 1900:
        cur = cand
    else:
        if cur: chunks.append(cur)
        cur = para[:1900]
if cur: chunks.append(cur)
fail = 0
for ch in chunks:
    payload = json.dumps({"target": f"channel:{channel}", "content": ch,
                          "source": "token-manager", "bot": "notify"}).encode()
    try:
        req = urllib.request.Request(api, data=payload, headers={"Content-Type": "application/json"})
        r = json.loads(urllib.request.urlopen(req, timeout=10).read())
        if not r.get("ok"):
            fail = 1; print(f"send not ok: {r}", file=sys.stderr)
    except Exception as e:
        fail = 1; print(f"send failed: {e}", file=sys.stderr)
sys.exit(fail)
PYSEND

if [[ "${1:-}" == "--no-send" ]]; then
  echo "=== FINAL (not sent) ==="; cat "$OUT_FILE"; exit 0
fi
"$PY" "$SENDER_PY" "$ADK_API" "$CHANNEL" "$OUT_FILE"
echo "Token daily report v2 sent"
