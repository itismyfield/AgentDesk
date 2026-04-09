---
description: 4계층 메모리에서 주제/키워드로 관련 지식을 검색한다. 파일 경로를 직접 참조하지 않고 의미 기반으로 접근하는 추상화 레이어. 예) /memory-read 전투 프레임워크, /memory-read --scope ltm architecture
---

# Memory Read — 메모리 추상 검색

## 목적
에이전트가 메모리에 접근할 때 파일 경로 대신 주제/키워드로 검색한다.
백엔드가 file, memento, mem0 중 무엇이든 사용자 입장에서는 같은 명령으로 동작해야 한다.

## 입력
`$ARGUMENTS`: 검색 쿼리 (필수)

옵션:
- `--scope {sam|sak|ltm|all}` — 검색 범위 (기본: all)
- `--agent {roleId}` — 특정 에이전트 메모리만 (기본: 현재 에이전트)
- `--limit {N}` — 최대 결과 수 (기본: 5)

## 백엔드 설정
우선 `~/.adk/release/config/agentdesk.yaml`의 `memory:` 섹션을 읽는다.
`memory:` 섹션이 없으면 `~/.adk/release/config/memory-backend.json`을 legacy fallback으로 읽는다.

예시:
```yaml
memory:
  backend: auto
  file:
    sak_path: "memories/shared-agent-knowledge/shared_knowledge.md"
    sam_path: "memories/shared-agent-memory"
    ltm_root: "memories/long-term"
    auto_memory_root: "~/.claude/projects/*{workspace}*/memory/"
  mcp:
    endpoint: "http://127.0.0.1:8765"
    access_key_env: "MEMENTO_API_KEY"
```

규칙:
- `backend`: `auto | memento | mem0 | file`
- `auto` 해석 순서: `memento` 가능 -> `mem0` 가능 -> 둘 다 불가면 `file`
- 명시 지정(`memento`, `mem0`, `file`)이면 자동감지를 건너뛴다
- `sam_path`는 단일 파일이 아니라 `{role_id}.json` 파일들이 저장되는 디렉토리 루트다

## 실행 전략

### 1. 백엔드 결정
1. `agentdesk.yaml`의 `memory:`를 읽고, 없으면 `memory-backend.json` fallback을 읽는다.
2. `backend=auto`면 `memento -> mem0 -> file` 순서로 사용 가능 여부를 판단한다.
3. `backend`가 명시돼 있으면 그대로 사용한다.

### 2. File 백엔드 검색

#### SAK 검색
1. `file.sak_path`를 읽는다.
2. 쿼리 키워드와 매칭되는 항목을 추출한다.

#### SAM 검색
1. `file.sam_path/{role_id}.json`의 `notes[]`를 읽는다.
2. 쿼리와 매칭되는 notes를 추출한다.

#### LTM 검색
1. `file.ltm_root` 아래 관련 `.md` 파일을 찾는다.
2. 파일명과 내용에서 관련 섹션만 추출한다.

### 3. MCP 백엔드 검색

#### memento
1. 연결된 Memento recall/search 계열 도구를 우선 사용한다.
2. 현재 턴에 MCP 도구가 없거나 런타임이 file fallback 상태임을 알려주면 file 경로를 보조 정본으로 다시 조회한다.

#### mem0
1. 연결된 Mem0 recall/search 계열 도구를 우선 사용한다.
2. 현재 턴에 MCP 도구가 없거나 검색 결과가 비면 file 경로를 보조 정본으로 조회한다.

## 결과 포맷
```markdown
## Memory Read 결과: "{쿼리}"

### SAK
- {매칭된 항목}

### SAM
- {매칭된 notes}

### LTM
- **{파일명}**: {관련 내용 요약}

### 메모
- backend: {file|memento|mem0}
- fallback: {없음|사유}
```

## 주의사항
- 파일 경로를 결과에 그대로 노출하지 않는다
- 결과가 없으면 "관련 기억 없음"으로 보고한다
- LTM 파일 전체를 복사하지 말고 관련 섹션만 추출한다
- `memento`/`mem0` 선택 시에도 현재 턴에 MCP가 없으면 file fallback을 허용하되, 한 줄로만 알린다
