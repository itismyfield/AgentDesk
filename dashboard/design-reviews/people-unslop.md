# 에이전트 관리 언슬롭

PR #7562 홈의 정보 위계를 에이전트 관리 화면에 적용했다. 기준은 Linear의 정렬된 목록, Vercel의 제목/행동 분리, Grafana의 요약→상세와 섹션 간격이다. 기준 공식 화면과 링크는 `home-overview.md`에 기록되어 있다.

## 화면과 동작

- 에이전트 3탭, 부서/상태/정렬/검색, grid/list 선택을 보존했다. 부서 칩 12개 대신 개수를 포함한 select를 쓰고 선택한 부서 편집을 명시적 동작으로 제공한다.
- 선택 전 비어 있던 PC 상세 열을 없애 목록이 전체 폭을 쓴다. 선택하면 PC는 목록/상세 2영역, 모바일은 선택 상세가 목록 앞에 나타난다. 선택 닫기를 추가했다. 한 열로 표시되는 폭에서는 카드 선택 직후 앱이 상세 패널로 이동하며, 이미 선택한 카드를 다시 눌러도 상세가 보인다.
- 목록은 이름/역할/현재 작업/관리 동작을 분리하고 긴 텍스트를 줄바꿈한다. 목록 항목의 키보드 진입도 지원한다.
- 부서 카드/순서 변경/생성 폼, 에이전트 수정 폼/생성 마법사 6단계, 직원 상세, 파견 인력/세션 상세를 같은 작은 모서리와 경계선으로 정리했다.
- 잘리던 모바일 마법사는 고정 헤더·푸터 사이의 본문이 스크롤한다. 프롬프트 파일 경로와 확인 화면은 긴 문자열을 감싼다. avatar 상태 점은 실제 avatar 경계를 따른다.
- 서버/API 계약과 실제 저장 동작은 바꾸지 않았다. 백로그 전용 재설계는 workflow 묶음이 담당한다.

## 확인 범위

390×844 / 1440×1000 × light/dark의 각 조합에서 확인했다.

- 기본 grid, 선택 상세, 직원 상세, 수정 폼의 상단/하단.
- list, 검색 결과 없음, 부서 목록/빈 부서, 부서 생성 폼.
- 에이전트 생성: 역할, Discord, 프롬프트, 작업공간, Cron, 확인.
- 파견 세션과 세션 상세. 긴 실제 session id와 실제 에이전트 이름을 사용했다.
- 새로고침 후 표시, 3탭 진입, 부서/제공자 필터, 별도 기본 실행 장치 저장은 기존 E2E fixture로 확인했다.

쓰기·메시지 전송·배포는 수행하지 않았다. Playwright는 GET을 실제 서버에서 읽고, 폼 점검 중 non-GET mutation은 브라우저에서 차단했다. WS ticket만 정상 연결을 위해 허용했다. 역할 맵에 없는 Discord 채널의 조회 오류가 시각 검토를 가리지 않도록 해당 채널 정보는 브라우저 fixture를 사용했다.

## 검증과 파일

- `npm run build` 성공.
- `PLAYWRIGHT_PORT=5275 npm run test:e2e -- --grep 'agents:' --reporter=line`: 4 passed, 2 viewport-specific skips.
- AgentFormModal/EmojiPicker/EmojiPickerLibraryPanel/AgentExecutionNodeSelect 단위 시험 15개 통과.
- Playwright CLI `verify-agents.js`: 기본 68개 상태와 모바일 자연 선택 4개 상태에서 폭 검사 실패 0건. 모든 기본/폼/마법사/세션 상태를 캡처하고 DOM 폭을 확인했다. 최초 검증의 선택 직후 강제 스크롤은 제거했다. 추가로 실제 390px에서 목록 하단 카드를 클릭하고 같은 카드를 다시 눌렀을 때 라이트/다크 모두 상세 상단이 105px에 나타나는지 확인했다. 검증 스크립트는 클릭 뒤 스크롤을 조작하지 않는다. 비활성 tooltip과 Vaul의 숨은 hit area는 실제 표시와 구분한다.
- `git diff --check`와 foundation 기준 PR cap 확인.

로컬 증거는 `output/playwright/people/`의 `before/`, `final/`, `verify-agents.js`, `layout-results.txt`, `natural-selection.js`, `natural-selection-results.txt`다. 자연 선택 결함 원본은 `natural-selection-before.png`에 별도로 남겼다. 캡처와 검증 스크립트는 별도 ZIP으로 전달하며 저장소에 PNG를 넣지 않는다. 머지·배포하지 않는다.


## 하위 화면 재감사와 추가 확인

nav/render 분기를 다시 대조해 최초 68개 캡처의 누락을 보완했다. 390/1440 × light/dark에서 상세 하위 상태 16종(64장), 채워진 생성 폼과 부서 순서 5종(20장)을 추가로 확인했다. 모바일 자연 선택 4장을 포함한 최종 캡처는 156장이다.

- 직원 상세: 별칭 편집, Discord/연결 세션, 크론·변경 로그, 긴 전용/공유 스킬 펼침, 활동 타임라인, 데이터 없음, 조회 실패. 최근 변경의 영구 로딩은 기존 audit GET을 연결하고 빈/실패 안내를 구분해 수정했다.
- 폼: 에이전트/부서 아이콘 선택, 부서 편집·삭제 확인, 에이전트 삭제 확인, 부서 순서 변경 초안. 실제 삭제/저장은 수행하지 않는다.
- 생성: 긴 프롬프트, 잘못된 Cron, 채워진 dry-run 결과와 오류·rollback. setup POST는 브라우저 fixture가 반환하고 실제 서버에 전달하지 않는다. Confirm은 실행하지 않는다.
- 추가 확인에서 38/42px select/input, 작은 아이콘 선택 조작, 다크 고정 picker, 모바일 picker 잘림·카테고리 잘림, 긴 스킬명 넘침을 수정했다.
- 기존 5개 에이전트/세션 모달은 body portal의 backdrop70/content80로 맞췄다. Toast65가 폼을 가리지 않으며 기존 포커스·Escape·닫기 동작을 유지한다. 실제 오류 toast가 남은 390px에서도 Preview를 자연 클릭해 확인했다.

`SessionPanel`의 disconnected 표시는 현재 nav로 도달할 수 없는 기존 분기다. `OfficeContext.tsx`의 `visibleDispatchedSessions`가 disconnected/linked를 제거한 뒤 라우트가 전달한다. fixture를 넣어도 버튼이 생기지 않는 것을 확인했으며 새 기능을 추가해 복구하지 않았다. routingOpen/sessionsOpen/idleSessionsOpen은 렌더에 사용되지 않는 기존 상태여서 별도 UI로 세지 않는다.

추가 재현: `verify-deep-agents.js` 64상태, `verify-advanced-agents.js` 20상태, 각각 가로 넘침/44px 결함 0건. 결과와 PNG는 `deep-final/`, `advanced-final/`에 있다. foundation650ab51ddd에서 build와 기존 E2E를 재확인했다. E2E 자동 preview 시작 30초 제한이 걸린 실행은 폐기하고, 같은 production build의 격리 preview5275를 재사용한 결과 4 passed/2 viewport skip이다.
