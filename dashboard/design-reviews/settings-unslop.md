# 설정·온보딩 디자인 검토

홈 #7562의 정보 위계와 간격 규칙을 설정 전체에 적용했다. 공통 shell/surface/overlay는 foundation #7564를 기반으로 한다. 머지·배포 없이 Draft PR에서 검토한다.

## 기준과 변경

- [Linear 목록](https://linear.app/docs/search)의 행 정렬: 설정의 이름·설명·조작을 동일한 축에 둔다. PC는 설명/컨트롤/상세 3열, 모바일은 설명 아래 컨트롤과 우측 상세 버튼이다.
- [Vercel 프로젝트 개요](https://vercel.com/blog/dashboard-redesign)의 위계: 페이지 제목 → 선택한 패널 설명 → 실제 설정 그룹 순서다. 중첩 외곽 카드를 제거하고 저장 동작은 상단에 둔다. 음성 설정의 중복 안내/저장 블록은 내용 뒤로 이동한다.
- [Grafana 개요](https://grafana.com/grafana/dashboards/1860-node-exporter-full/)의 밀도: 섹션 경계와 항목 간격을 구분한다. 8px 모서리·구분선·중립 배경을 사용하고 상태에만 의미 있는 색을 남긴다.

900px 미만은 8개 패널을 4열×2행으로 모두 노출한다. 900px 이상은 검색과 수직 메뉴를 좌측에 두고 내용 영역을 넓힌다. URL의 `settingsPanel`과 기존 저장 상태, 검색, 모든 탭·필드·동작을 유지했다.

설정 row의 상세 버튼과 switch, 음성 TTL 입력·에이전트 switch, 프로바이더 계정 동작을 최소 44px로 보강했다. 설정 metadata는 raw flag 대신 읽기 전용/재시작 필요 등 사용자 문구와 테마 대비를 사용한다. 런타임 API 실패 시 빈 값을 정상 설정처럼 표시하지 않고 실패 안내를 보여준다.

온보딩은 테마를 무시하던 고정 dark overlay를 제거했다. 5단계 rail은 가로 스크롤·잘림 없이 전체를 보여준다. 반복된 진행 막대를 줄이고 제목/현재 단계 → 복원 안내 → 단계 → 폼 순서다. checklist는 중첩 색상 카드 대신 행과 구분선으로 읽는다. 에이전트 프롬프트 펼치기는 실제 키보드 버튼이며 삭제 동작을 분리했다. 도움말은 44px 버튼으로 열고 닫으며 화면 안에 표시한다.

## 확인한 화면과 상태

- 일반: 회사/CEO/언어/테마 폼, 행 상세 정보, 검색 일치·검색 결과 없음, 저장 disabled/dirty.
- 머신: 실데이터 Hub/Runner 목록·자원 이력·장치 상세·실행 준비 상태, 기존 E2E의 stale/error/offline/단독 운영 fixture.
- 런타임: 디스패치 제한·자동 큐·알림 임계값·캐시 TTL 4개 accordion, 숫자/range/switch·기본값 복원, API 실패 안내.
- 파이프라인: 대상 저장소·에이전트 선택, 기본/고급 설정, 위험 설정 저장 확인 모달. 내장 흐름 편집기 자체는 workflow 묶음 소유이며 별도 PR에서 44px/캔버스/상세 폼을 검증한다.
- 음성: 전체 설정, STT/TTS 모델 폼, 선택 에이전트의 이름/호출 이름/프롬프트, 에이전트 없음·API 실패.
- 프로바이더: 기본/추가 계정, 긴 profile/path/연결 정보, 사용량, 추가 로그인 진행 상태, 계정 없음·API 실패.
- 커넥터: 요약·목록·긴 이름/환경변수/경로/설명·설정 작업, 비어 있음·API 실패.
- 온보딩 설정: 초기 연결 정보와 재실행 overlay. wizard 1 봇 연결 / 2 프로바이더 확인 / 3 역할·에이전트 / 4 채널 / 5 소유자·적용. 복원 안내·도움말·긴 이름과 설명을 포함한다.

## Playwright 확인 루프

실제 Vite React 페이지를 실행 중인 서버의 읽기 데이터로 확인했다. 일반 화면은 실데이터이며, 빈 상태/API 실패/긴 연속 문자열/계정 로그인/위험 설정 모달/온보딩 초안은 브라우저 응답 fixture다. 모든 실제 쓰기 요청은 차단했고 설정 저장·로그인 세션·Discord 채널 생성·온보딩 적용은 수행하지 않았다.

390×844 / 1440×1000 각각 light/dark를 캡처했다. 주요 목록의 위·아래를 함께 찍고 직접 열어 정렬·간격·잘림·하단 가림을 확인했다. 첫 검사에서 발견한 음성 TTL 38px, 계정 unlink/primary 작은 타깃, light 커넥터 배지 대비, 온보딩 rail 잘림·light 대비와 카드 header 키보드 접근을 수정한 뒤 다시 찍었다.

기존 모바일 E2E의 가로 스크롤 메뉴 기대는 전체 탭 노출/겹침 없음으로 갱신했고, 머신 개수 확인은 모바일에서 숨기는 메뉴 count 대신 선택 패널의 보이는 count를 확인한다.

캡처 루트: worktree의 `output/playwright/settings-unslop/`.

- `before/`: 기존 8패널 × 4 viewport/theme 조합.
- `final/`: 새 8패널 위/아래와 행 상세·검색 없음·런타임 category 88장. overflow/44px 검사 88회 결함 0건.
- `onboarding-before/`: 5단계 × 4 조합. `onboarding-after/`: 위/아래 40장과 프롬프트 펼침·도움말 8장.
- `states/`: 빈 상태·오류·긴 데이터·로그인 진행·위험 설정 확인 48장.
- `verify-settings.js`, `verify-states.js`, `capture-onboarding-after.js`: 재현 스크립트.

## 검증

- `npm --prefix dashboard run build` — 성공
- `npm --prefix dashboard run test -- settings onboarding` — 9개 파일, 44개 통과
- `PLAYWRIGHT_PORT=5276 npm --prefix dashboard run test:e2e -- --grep 'settings' --reporter=line` — 13개 통과, desktop/mobile 전용 3개 의도된 skip
- `git diff --check`

API/WS/schema와 운영 설정을 변경하지 않는다. 초기 화면의 연결 끊김 배지와 preview Origin 문제는 foundation #7564에서 해결한다.
