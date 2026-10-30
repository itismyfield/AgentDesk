# 에이전트 관리 언슬롭

PR #7562 홈의 정보 위계를 에이전트 관리 화면에 적용했다. 기준은 Linear의 정렬된 목록, Vercel의 제목/행동 분리, Grafana의 요약→상세와 섹션 간격이다. 기준 공식 화면과 링크는 `home-overview.md`에 기록되어 있다.

## 화면과 동작

- 에이전트 3탭, 부서/상태/정렬/검색, grid/list 선택을 보존했다. 부서 칩 12개 대신 개수를 포함한 select를 쓰고 선택한 부서 편집을 명시적 동작으로 제공한다.
- 선택 전 비어 있던 PC 상세 열을 없애 목록이 전체 폭을 쓴다. 선택하면 PC는 목록/상세 2영역, 모바일은 선택 상세가 목록 앞에 나타난다. 선택 닫기를 추가했다.
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
- AgentFormModal 단위 시험 통과.
- Playwright CLI `verify-agents.js`: 모든 기본/폼/마법사/세션 상태를 캡처하고 DOM 폭을 확인했다. 비활성 tooltip과 Vaul의 숨은 hit area는 실제 표시와 구분한다.
- `git diff --check`와 foundation 기준 PR cap 확인.

로컬 증거는 `output/playwright/people/`의 `before/`, `final/`, `verify-agents.js`, `layout-results.txt`다. 캡처와 검증 스크립트는 별도 ZIP으로 전달하며 저장소에 PNG를 넣지 않는다. 머지·배포하지 않는다.
