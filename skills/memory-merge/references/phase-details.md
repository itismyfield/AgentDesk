# Phase 상세 — 수집, 위생정리, 작성, 초기화

## Phase 1: 수집
1. `memory-backend.json`의 `file.auto_memory_root` 패턴을 기준으로 `*.md` 파일을 수집한다
2. `MEMORY.md`는 인덱스 파일로만 취급하고 본문 merge 대상에서는 제외한다
3. 각 파일을 읽고 출처 워크스페이스와 roleId를 기록한다
4. 파일이 없으면 "병합 대상 없음"으로 보고하고 종료한다

## Phase 2: 위생 정리
1. 상대 날짜를 절대 날짜로 바꾼다
2. 같은 주제의 충돌 기록은 최신 항목만 남긴다
3. 삭제된 파일/이미 끝난 이슈를 가리키는 stale 항목을 제거한다
4. 중복 항목을 합친다
5. MEMORY.md 인덱스와 실제 파일의 불일치를 정리한다

## Phase 3: 분류
1. 각 항목을 SAM / SAK / LTM / System Prompt로 분류한다
2. 동시에 내용 성격을 fact / decision / error / procedure / preference / relation으로 태깅한다
3. backend가 `memento` 또는 `mem0`일 때는 이 태그를 sink 분기에 사용한다

## Phase 4: sink backend 결정
1. `backend`를 읽는다
2. `auto`면 `memento -> mem0 -> file` 순서로 감지한다
3. 명시 지정이면 그대로 사용한다

## Phase 5: file backend 기록
`backend=file`일 때만 수행한다.

### 5-1. SAK 작성
1. `file.sak_path`를 읽는다
2. SAK 항목을 중복 체크 후 병합한다
3. 80줄 이내를 유지한다

### 5-2. SAM 작성
1. `file.sam_path/{role_id}.json`을 읽는다
2. `notes[]`에 항목을 기록한다
3. notes는 최대 10건 유지한다

### 5-3. LTM 작성
1. `file.ltm_root/{roleId}/` 아래 기존 파일을 찾는다
2. 있으면 보강하고 없으면 새 파일을 만든다

## Phase 6: MCP backend 기록
`backend=memento | mem0`일 때는 파일로 쓰지 않고 아래 규칙을 따른다.

### memento
- fact / decision / error / procedure만 Memento MCP에 기록
- preference / relation은 skip

### mem0
- preference / relation만 Mem0 MCP에 기록
- fact / decision / error / procedure는 skip

현재 턴에 필요한 MCP 도구를 사용할 수 없으면 file로 몰래 대체하지 말고 `skip + 이유`를 보고한다.

## Phase 7: System Prompt 반영
1. System Prompt 항목은 직접 수정하지 않는다
2. 보고서에 "System Prompt 반영 검토 필요"로만 남긴다

## Phase 8: 초기화
`--dry-run`이 아닐 때:
1. 처리 완료된 원본 auto memory 파일을 백업 후 삭제한다
2. 각 워크스페이스의 `MEMORY.md` 인덱스를 비우거나 정리한다
3. 백업 위치는 file 계층 archive 루트를 따른다
