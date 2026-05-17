#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

cat >"$PROMPT_FILE" <<PROMPT
Memento 일일 리포트를 생성한다. 현재 시각: $NOW_KST

## 수집할 데이터

1. memory_stats 호출 → 전체 현황
2. search_traces로 어제(KST 기준) 생성된 파편 조회 (limit=100)

## 리포트 포맷 (한국어, Discord 메시지)

**Memento 일일 리포트 — {날짜}**

전체 현황:
- 총 파편: N개 (permanent N / hot N / warm N / cold N)
- 유형별: fact N / decision N / error N / procedure N / preference N / episode N
- 평균 중요도: 0.XX

어제 활동:
- 신규 파편: N건
- 유형별 내역: fact N, decision N, error N, episode N, ...
- 주요 토픽: (상위 5개 나열)

검색 품질 (30일):
- 총 검색: N회
- L1 미스율: XX%
- L3 사용률: XX%
- 평균 지연: NNms
- tool_feedback 제출: N건

미해결 에러: N건 (있으면 토픽 나열)

반드시 위 포맷 그대로 Discord 메시지로 반환. NO_REPLY 금지 — 항상 리포트를 생성한다.
PROMPT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "memento-daily-report" \
  --target "channel:1480015244062490774" \
  --workdir "/Users/itismyfield/.adk/release/workspaces/agentfactory" \
  --prompt-file "$PROMPT_FILE"
