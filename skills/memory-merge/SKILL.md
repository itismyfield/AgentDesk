---
description: 각 에이전트 워크스페이스의 Claude Code memory 파일을 위생 정리(Auto Dream 대체) + 4계층 분배한다. 하루 1회 Scheduler가 자동 실행하거나 수동으로 호출. 예) /memory-merge, /memory-merge --dry-run
---

# Memory Merge — 메모리 위생 정리 + 4계층 분배기

## 목적
Claude Code의 auto memory 파일을 수집해 위생 정리하고, 4계층 메모리 또는 MCP backend로 분배한다.

이 스킬의 핵심 규칙:
- `source`는 항상 file 기반 auto memory
- `sink`만 `backend`에 따라 달라진다

## 옵션
- `$ARGUMENTS`에 `--dry-run`이 포함되면 실제 파일 수정 없이 분석 결과만 출력한다

## 백엔드 설정
우선 `~/.adk/release/config/agentdesk.yaml`의 `memory:` 섹션을 읽는다.
`memory:` 섹션이 없으면 `~/.adk/release/config/memory-backend.json`을 legacy fallback으로 읽는다.

규칙:
- `source`: 항상 `file.auto_memory_root`
- `sink`: `backend` 해석 결과에 따라 `file | memento`
- `backend=auto`면 `memento -> file` 순서로 감지
- 명시 지정(`memento`, `file`)이면 자동감지 skip

## 실행 전략
1. **Phase 1: 수집** — `file.auto_memory_root`에서 memory 파일 수집, 출처 워크스페이스 매핑
2. **Phase 2: 위생 정리** — 상대날짜 변환, 모순 제거, stale 제거, 중복 합침, 인덱스 정합성 확인
3. **Phase 3: 분류** — 각 항목을 SAM / SAK / LTM / System Prompt / MCP 대상 성격으로 분류
4. **Phase 4: sink backend 결정** — `file | memento`
5. **Phase 5: backend별 기록**
   - `file`: SAK / SAM / LTM 파일에 기록
   - `memento`: fact / decision / error / procedure만 Memento MCP에 기록, preference / relation은 skip
6. **Phase 6: System Prompt 반영 대상 분리** — 직접 수정하지 않고 검토 목록만 생성
7. **Phase 7: 초기화** — 원본 auto memory 파일 백업 후 삭제, MEMORY.md 인덱스 정리
8. **Phase 8: 보고** — 위생 정리 + 분배 결과 요약 출력

## 주의사항
- `file` backend일 때만 SAK 80줄 / SAM notes 10건 제한을 적용한다
- `memento` backend에서 현재 턴에 MCP를 쓸 수 없으면 file로 몰래 바꾸지 말고 `skip + 이유`를 보고한다
- System Prompt는 자동 수정 금지, 보고만 한다
- 코드/설정에 이미 반영된 내용은 중복 저장하지 않는다
- merge 대상이 없으면 `skip`으로 끝낸다

> 상세 규칙은 `references/architecture.md`, `references/classification-guide.md`, `references/phase-details.md`, `references/report-template.md`를 따른다
