#!/bin/bash
# Migrated from launchd: com.itismyfield.memento-hygiene
# Owner routine agent: project-agentmanager (workspace agentfactory)
# Schedule: 0 3 * * * (KST, 03:00 daily). Sunday(=7) additionally runs the
# deterministic cross-workspace deep pass (memento-deep-cleanup.sql).
#
# Design (2026-07-15):
#  - Daily light pass: in-scope MCP hygiene + store-health monitor (LLM job).
#  - Sunday deep pass: deterministic SQL cross-workspace cleanup with hard
#    guards + cap 1500, run by THIS wrapper (NOT the LLM). LLM only reports it.
#  - DB access is node-agnostic: local psql on the memento host, ssh otherwise.
set -euo pipefail

PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

# --- Sunday deep pass: deterministic cross-workspace cleanup (safe, bounded) ---
run_deep_cleanup() {
  local sqlf="$SCRIPT_DIR/memento-deep-cleanup.sql"
  if [ -f "$HOME/memento-mcp/.env" ]; then
    # memento DB is local on this node (mac-mini)
    ( set -a; . "$HOME/memento-mcp/.env"; set +a; psql "$DATABASE_URL" -qtA -f "$sqlf" )
  else
    # remote node: pipe the SQL to psql on the memento host over ssh
    ssh mac-mini 'set -a; . ~/memento-mcp/.env; set +a; psql "$DATABASE_URL" -qtA' < "$sqlf"
  fi
}

DEEP_REPORT="스킵 (일요일 아님 — 심화 cross-workspace 패스는 일요일만 수행)"
if [ "$(TZ='Asia/Seoul' date +%u)" = "7" ]; then
  if DEEP_OUT=$(run_deep_cleanup 2>&1); then
    DEEP_REPORT="$DEEP_OUT"
  else
    DEEP_REPORT="심화 패스 실행 오류(삭제 미수행 가능):
$DEEP_OUT"
  fi
fi

cat >"$PROMPT_FILE" <<PROMPT
Memento 위생 관리(in-scope 경량 + 저장소 모니터)를 수행한다. 아래 순서로 실행.

## 절대 규칙 (하드 가드레일 — 위반 시 즉시 중단)
- DB 직접 접근 절대 금지: psql / SSH / SQL 등 어떤 DB 직접 조작도 하지 않는다. 오직 Memento MCP 도구(recall / forget / amend / memory_consolidate / memory_stats / context)만 사용한다.
- cross-workspace 삭제 절대 금지: 현재 에이전트 스코프 밖(다른 에이전트 소유) 또는 RLS로 forget이 차단되는 파편은 절대 삭제하지 않는다. (cross-workspace 심화 정리는 이 루틴의 래퍼 스크립트가 결정론적 SQL로 이미 처리했다 — 아래 '심화 패스 결과' 참조. LLM은 관여하지 않는다.)
- 미수행 작업 보고 금지: 실제 실행하여 success 응답을 받은 작업만 보고한다. 삭제 건수는 forget이 success를 반환한 건만 집계한다. 실행하지 않은 작업을 지어내지 않는다.

## 1단계: Memento consolidation
memory_consolidate를 호출한다 (TTL 전환·중요도 감쇠·만료 삭제·중복 병합). 권한(master key) 없어 실패하면 에러를 그대로 보고한다.

## 2단계: in-scope 저가치 스윕 (자기 스코프만, forget)
recall로 자기 스코프 파편을 조회하고 아래 기준에 해당하면 forget(필요 시 force=true).
- error: age>7d, 또는 resolutionStatus=resolved, 또는 assertionStatus=rejected, 또는 importance<0.3, 또는 resolved_by 링크 존재(graph_explore 확인)
- episode: topic=session_reflect & importance<0.6 & age>1d; 또는 resolution_status=resolved & age>2d
- decision: topic=session_reflect & content에 숫자(이슈/포트/버전)·파일경로·도구명이 하나도 없는 추상 문장 & importance<=0.7 & age>1d
- fact: topic=session_reflect & importance<=0.5; 또는 content에 현황/status/상태 포함 & age>3d
보존(삭제 금지): isAnchor=true, importance>=0.8, 현재 스코프 밖 파편.
삭제 전 "원인→해결" 패턴이 명확한 error는 procedure 파편으로 remember한 뒤 삭제.

## 3단계: 저장소 건강 모니터 (읽기 전용)
- memory_stats 호출 → 총 파편수, type별 분포(특히 episode 비중), avg_importance 확인.
- context(structured=true) 호출 → 자기 스코프 anchorCount 확인.
- 경보 판단:
  · episode 비중이 과도(예: 전체의 절반 이상)하거나 총 파편수가 급증 → "deprecation-audit 심화 권장" 경보
  · anchorCount가 주입 한도(약 10)를 크게 초과 → "앵커 인플레 — de-anchor 필요" 경보

## 심화 패스 결과 (래퍼가 수행, 일요일만 — cross-workspace 결정론적 SQL)
아래는 이번 실행의 심화 패스 원시 출력이다. DEEP_CANDIDATES/DEEP_BYTYPE/DEEP_DELETED 값을 그대로 해석해 보고에 반영한다(네가 삭제한 게 아니라 래퍼가 SQL로 삭제한 것이다):
---
${DEEP_REPORT}
---

## 결과 보고 (한국어, Discord)
- Consolidation: 요약(또는 권한오류)
- in-scope 위생: error N / episode N / decision N / fact N 삭제
- 저장소 건강: 총 파편 X · episode 비중 Y% · anchorCount Z (+경보 여부)
- 심화 패스(일요일): 삭제 N건(type별) / 또는 "스킵"
정리·경보 내용이 하나도 없으면 NO_REPLY를 반환.
PROMPT

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "memento-hygiene" \
  --target "channel:1480015244062490774" \
  --workdir "$AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR" \
  --prompt-file "$PROMPT_FILE"
