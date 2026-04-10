# 4계층 메모리 아키텍처 — 각 계층 상세

## file backend 기준 저장 위치

**SAM** — `agentdesk.yaml`의 `memory.file.sam_path/{role_id}.json` (`memory:`가 없으면 legacy `memory-backend.json` fallback)
- 해당 에이전트(role)에만 주입되는 휘발성 컨텍스트
- dcserver가 동일 role의 다른 provider/채널 세션에 자동 주입
- 용도: 진행 중 작업 상태, 임시 결정, 세션 간 인수인계

**SAK** — `agentdesk.yaml`의 `memory.file.sak_path` (`memory:`가 없으면 legacy `memory-backend.json` fallback)
- 전체 에이전트에 주입되는 휘발성 교차 지식
- 용도: 최근 프로젝트 결정, 공통 규칙 변경, 외부 참조
- 80줄 제한

**LTM** — `agentdesk.yaml`의 `memory.file.ltm_root/{roleId}/*.md` (`memory:`가 없으면 legacy `memory-backend.json` fallback)
- 특정 에이전트의 전문 영역 지식
- 자동 주입이 아니라 필요 시 읽는 장기 기억

**System Prompt**
- 전체 또는 특정 에이전트에 항상 주입되는 영구 규칙
- 자동 수정하지 않고 검토 대상으로만 올린다

## backend별 sink 규칙

**file**
- SAK / SAM / LTM를 모두 파일에 기록한다

**memento**
- fact / decision / error / procedure만 Memento MCP에 기록한다
- preference / relation은 이번 이슈 범위에서 skip한다

**mem0**
- preference / relation만 Mem0 MCP에 기록한다
- fact / decision / error / procedure는 이번 이슈 범위에서 skip한다
