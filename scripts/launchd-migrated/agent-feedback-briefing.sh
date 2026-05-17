#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

cat >"$PROMPT_FILE" <<EOF
에이전트 피드백 브리핑을 실행한다.
현재 시각: $NOW_KST

절차:
1. /Users/itismyfield/ObsidianVault/RemoteVault/agents/ch-pmd/inbox/ 디렉토리의 모든 .md 파일을 읽는다
2. pending 상태인 항목을 모두 수집한다 (오늘 + 과거 미결)
3. 접수건이 0건이면 NO_REPLY를 반환한다
4. 1건 이상이면 아래 포맷으로 브리핑을 작성한다:

<@1479017284805722200>

## 에이전트 피드백 브리핑 — {오늘 날짜}

### 오늘 접수 ({N}건)
| # | 카테고리 | 출처 | 요약 | 관련 이슈 |
|---|---------|------|------|----------|
| 1 | {카테고리} | {출처} | {요약} | #{번호} |

### 상세
**1. [{카테고리}] {제목}**
- 출처: {에이전트}
- 내용: {설명}
- 제안: {있으면}

### 판단 요청
위 건들에 대해 승인/거부/보류를 알려주세요.

5. 결정 완료(approved/rejected/deferred) 후 7일 경과한 항목은 파일에서 삭제한다
6. 파일 내 모든 항목이 삭제되면 파일 자체를 삭제한다

Rules:
- Return only the final Korean Discord message ready to send, or NO_REPLY if no pending items exist.
- Do not send the message yourself.
- Do not wrap the final answer in code fences.
- <@1479017284805722200> is the user mention — always include at the top.
EOF

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "agent-feedback-briefing" \
  --target "channel:1478652416533463101" \
  --prompt-file "$PROMPT_FILE"
