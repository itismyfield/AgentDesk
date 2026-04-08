---
description: 4계층 메모리에 지식을 저장한다. 저장 계층(SAM/SAK/LTM)을 자동 판단하거나 명시적으로 지정 가능. 파일 경로를 직접 다루지 않는 추상화 레이어. 예) /memory-write --scope sak "ADK 포트가 8791로 변경됨", /memory-write 전투 시스템 GA 패턴 정리
---

# Memory Write — 메모리 추상 저장

## 목적
에이전트가 메모리에 저장할 때 파일 경로 대신 주제/내용과 계층을 지정한다.
백엔드가 file, memento, mem0 중 무엇이든 사용자 명령과 보고 형식은 동일해야 한다.

## 입력
`$ARGUMENTS`: 저장할 내용 (필수)

옵션:
- `--scope {sam|sak|ltm}` — 저장 계층 (생략 시 자동 판단)
- `--agent {roleId}` — 대상 에이전트 (기본: 현재 에이전트)
- `--topic {주제}` — LTM 저장 시 토픽
- `--ttl {days}` — SAM 만료 기한 (기본: 7일)

## 자동 계층 판단
`--scope` 미지정 시 내용을 분석해 자동 분류한다.

| 판단 기준 | 계층 |
|---|---|
| 전체 에이전트 관련 + 휘발성 | SAK |
| 현재 에이전트만 + 휘발성 | SAM |
| 현재 에이전트만 + 영구적 | LTM |
| 전체 에이전트 + 영구적 | System Prompt 반영 검토로 보고 |

## 백엔드 설정
`~/.adk/release/config/memory-backend.json`을 읽는다.

규칙:
- `backend`: `auto | memento | mem0 | file`
- `auto` 해석 순서: `memento` 가능 -> `mem0` 가능 -> 둘 다 불가면 `file`
- 명시 지정(`memento`, `mem0`, `file`)이면 자동감지를 건너뛴다
- `sam_path`는 `{role_id}.json` 파일들이 저장되는 디렉토리 루트다

## 실행 전략

### 1. 백엔드 결정
1. `memory-backend.json`을 읽는다.
2. `backend=auto`면 `memento -> mem0 -> file` 순서로 사용 가능 여부를 판단한다.
3. 명시 지정이면 그대로 사용한다.

### 2. File 백엔드 저장

#### SAK
1. `file.sak_path`를 읽는다.
2. 기존 항목과 중복 체크 후 병합한다.
3. 80줄 제한을 넘기지 않도록 오래된 항목 정리를 같이 수행한다.

#### SAM
1. `file.sam_path/{role_id}.json`을 읽는다.
2. `notes[]`에 새 항목을 추가하거나 기존 항목을 갱신한다.
3. notes는 최대 10건을 유지한다.

#### LTM
1. `file.ltm_root/{roleId}/` 아래 관련 파일을 찾는다.
2. 기존 파일이 있으면 보강하고, 없으면 새 파일을 만든다.

### 3. MCP 백엔드 저장

#### memento
1. fact / decision / error / procedure 성격의 내용이면 연결된 Memento write 계열 도구를 우선 사용한다.
2. 현재 턴에 MCP 도구가 없거나 저장 실패 시 file backend로 fallback하고 짧게 보고한다.

#### mem0
1. preference / relation / profile 성격의 내용이면 연결된 Mem0 write 계열 도구를 우선 사용한다.
2. 현재 턴에 MCP 도구가 없거나 현재 내용이 Mem0 성격과 맞지 않으면 file backend로 fallback하고 짧게 보고한다.

## 결과 포맷
```markdown
## Memory Write 완료
- 계층: {SAM|SAK|LTM}
- 주제: {topic}
- 동작: {추가|갱신|새 생성|skip}
- backend: {file|memento|mem0}
- fallback: {없음|사유}
```

## 주의사항
- System Prompt 계층은 직접 저장하지 않고 보고만 한다
- SAK 80줄, SAM 10건 제한을 항상 검사한다
- 동일 내용이 이미 존재하면 중복 저장하지 않는다
- 파일 경로를 결과에 직접 노출하지 않는다
- `memento`/`mem0` 선택 시에도 현재 턴에 MCP가 없으면 file fallback을 허용하되, fallback 사실을 짧게 보고한다
