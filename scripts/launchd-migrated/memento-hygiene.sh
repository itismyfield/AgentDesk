#!/bin/bash
set -euo pipefail

PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

cat >"$PROMPT_FILE" <<'PROMPT'
Memento 위생 관리 작업을 수행한다. 아래 3단계를 순서대로 실행.

## 1단계: Claude/Codex 로컬 메모리 이관 검토

~/.claude/projects/ 하위 모든 프로젝트의 memory/ 디렉토리와 MEMORY.md를 스캔한다.
~/.codex/memories/ 도 스캔한다.

파일이 있으면:
- 각 파일 내용을 읽고, Memento에 이미 존재하는 정보인지 recall로 확인
- 중복이 아닌 유효한 정보는 remember로 Memento에 저장 (적절한 type/topic/importance 부여)
- 이관 완료 후 해당 파일 삭제
- 파일이 없으면 "로컬 메모리 비어있음" 보고

## 2단계: Memento consolidation

memory_consolidate를 호출한다. TTL 전환, 중요도 감쇠, 만료 삭제, 중복 병합이 자동 수행된다.

## 3단계: 미해결 에러 자동 정리

recall로 type=error 파편을 전부 조회한다 (resolutionStatus 무관, pageSize=50).

**삭제 규칙 (forget)** — 하나라도 해당하면 삭제:
1. 생성 7일 이상 경과한 error 파편 (timeRange.to = 7일 전)
2. resolutionStatus가 "resolved"인 파편
3. assertionStatus가 "rejected"인 파편
4. importance가 0.3 미만인 파편
5. resolved_by 링크가 있는 파편 (graph_explore로 확인)

**보존 규칙** — 아래 조건은 삭제하지 않음:
- isAnchor=true인 파편
- importance 0.8 이상이면서 생성 3일 이내인 파편

삭제 전 각 파편의 content에서 핵심 교훈이 있으면 procedure 파편으로 remember한 뒤 삭제.
교훈 추출 기준: "원인→해결" 패턴이 명확한 경우만.

정리 건수와 보존 건수를 보고.

## 4단계: 저가치 파편 정리

recall로 아래 카테고리별 파편을 조회하고 삭제 규칙에 따라 forget(force=true) 처리.

### 4-1. 에피소드 쓰레기
- topic="session_reflect", type=episode 중 importance < 0.6이고 age > 1일인 파편 삭제
  (세션 통계, 도구 사용 횟수만 기록된 내용 없는 파편)
- type=episode, resolution_status="resolved"이고 age > 2일인 이슈 회고 에피소드 삭제
  (완료된 이슈의 "성공 종료" 기록은 이슈/커밋에 이미 남아있으므로 불필요)

### 4-2. 모호한 결정 정리
- topic="session_reflect", type=decision 중 content가 구체적 설정값·파일경로·이슈번호 없이
  추상적 문장만 있는 파편 삭제 (예: "~를 채택했다", "~로 결정했다"만 있고 what/where/how 없음)
- 판단 기준: content에 숫자(이슈번호·포트·버전), 파일경로, 구체적 도구명이 하나도 없으면 삭제 후보
- importance 0.7 이하이고 age > 1일인 것만 대상

### 4-3. 메타 쓰레기 사실 정리
- topic="session_reflect", type=fact 중 importance ≤ 0.5인 파편 삭제
  (예: "파편 N개를 forget으로 제거했다", "git log --grep 명령어를 활용했다" 같은 메타 기록)

### 4-4. 오래된 스냅샷 사실 정리
- type=fact 중 "현황", "status", "상태" 키워드가 content에 포함되고 age > 3일인 파편 삭제
  (시점 스냅샷은 3일 지나면 stale)

**보존 규칙** — 아래는 절대 삭제하지 않음:
- isAnchor=true
- workspace가 현재 에이전트 스코프 밖인 파편 (다른 에이전트 소유)
- importance 0.8 이상

4단계 정리 건수를 카테고리별로 보고.

## 결과 보고

Discord 메시지로 아래 포맷의 한국어 요약을 반환:
- 로컬 메모리 이관: N건 이관 / M건 중복 스킵 / 비어있음
- Consolidation: 결과 요약
- 에러 정리: N건 정리 / M건 미해결 유지
- 파편 위생: 에피소드 N건 / 결정 N건 / 사실 N건 삭제

내용이 없으면 NO_REPLY를 반환.
PROMPT

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "memento-hygiene" \
  --target "channel:1480015244062490774" \
  --workdir "$AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR" \
  --prompt-file "$PROMPT_FILE"
