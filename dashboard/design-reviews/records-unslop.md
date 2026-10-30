# 회의·스킬·업적 언슬롭

홈 PR #7562에서 확인한 Linear 목록 정렬, Vercel 정보 위계, Grafana 요약→상세 원칙을 적용했다. 공식 기준 화면과 링크는 `home-overview.md`를 참조한다.

## 바뀐 구조

- 회의는 페이지 제목/주요 행동 → 검색과 상태 필터 → 선택 회의의 요약·후속 액션·발췌 순이다. PC는 기록/상세의 정렬된 2영역, 모바일은 회의/스킬을 분리해 세로로 읽는다.
- 빈 회의 상태에서 같은 빈 상세와 빈 타임라인을 반복하지 않는다. 회의 시작과 기록 검토·후속 액션 기능은 유지한다.
- 회의/스킬의 커스텀 CSS 3개(총 961줄)를 실제 사용하는 Tailwind 유틸리티로 옮겼다. 시안이나 구현 이력을 설명하던 제품 문구도 운영 설명으로 바꿨다.
- 업적은 큰 장식 배너와 다색 배경을 줄이고 대표 레벨·XP 요약, 획득/진행/잠김 목록, 미션·연속 활동·랭킹으로 정렬했다. 모든 업적과 상세/에이전트 진입을 보존한다.
- 업적/회의 상세, 새 회의 폼, 스킬 카테고리·검색·빈 결과도 작은 모서리, 얇은 경계, 중립 메타데이터를 사용한다. 모달 닫기와 동작의 터치 영역은 44px 이상이다.

## 화면 확인 루프

390×844 / 1440×1000 × light/dark에서 다음 상태를 캡처하고 직접 확인했다.

- 회의 목록/선택/후속 액션/전사, 상태 필터와 검색 결과 없음.
- 회의 상세 모달, 새 회의 작성 폼(실제 시작/저장은 하지 않음).
- 스킬 목록/긴 연속 식별자/검색 결과 없음/빈 카탈로그.
- 업적 기본/획득 상세/진행 상세/미션·streak·랭킹/빈 섹션.
- 회의 데이터 없음, 스킬 데이터 없음.

실행 서버의 회의 목록은 비어 있어 원본 빈 상태를 먼저 보관했다. 채워진 회의와 스킬은 기존 API 형태를 따르는 브라우저 fixture를 사용했으며, 업적은 실행 서버의 실제 데이터를 사용했다. UI 점검 동안 실제 mutation은 브라우저에서 차단했다. WS 연결과 GET만 허용하고 회의 시작·이슈 생성·메시지 전송·설정 저장은 하지 않았다.

캡처 검사는 화면 폭, 긴 텍스트, 44px 주요 동작, 모달 애니메이션 완료, 하단 탭바 여유를 확인한다. Vaul의 의도된 비표시 drag hit area는 텍스트 넘침과 구분한다.

## 검증과 증거

- `npm run build` 성공.
- `PLAYWRIGHT_PORT=5276 npm run test:e2e -- --grep 'meetings:|achievements:' --reporter=line`: 9 passed, 3 viewport-specific skips.
- MeetingDetailModal/MeetingMinutesView 관련 단위 시험 18개 통과.
- Playwright CLI `verify-records.js`: 48개 상태 캡처와 폭 확인, 실패 0건.
- `git diff --check`, foundation 기준 PR cap 확인.

로컬 캡처는 `output/playwright/records/`의 `before/`, `final/`, `verify-records.js`, `layout-results.txt`에 있다. PNG와 스크립트는 별도 ZIP으로 전달하며 소스 PR에 넣지 않는다. 머지·배포하지 않는다.


## 하위 분기 재감사와 추가 확인

390/1440 × light/dark에서 최초 48장에 추가로 20개 하위 상태(80장)를 확인했다. 회의 생성됨/폐기됨/실패/대기 후속 이슈 각각의 본문·액션, 저장소 선택/조회 실패, 채널 ID 편집, 전문가 검색 없음/선택, 스킬 5개 카테고리, 잠긴 업적, 업적→직원, 업적/지원 섹션 없음과 API 실패를 포함한다. API 실패가 달성0처럼만 보이던 업적은 실패 안내를 표시한다.

새 분기에서 발견한 회의 삭제·미리보기·저장소 select의 작은 타깃과 라이트 상태 대비를 수정했다. 회의 상세 모달은 backdrop70/content80 body portal로 맞추고 기존 Escape/포커스 동작과 provider flow SSR 계약을 유지했다. 업적의 에이전트를 누르면 기존 Drawer를 닫은 뒤 직원 상세 하나만 보이도록 수정했다.

직원 상세는 agents 형제 묶음 소유다. 이 PR 단독의 기존 AgentInfoProfileSections select38px는 #7565가 44px로 바꾸므로 중복 편집하지 않았다. 이 진입의 최종 4개 캡처/터치 계측은 모든 묶음을 포함한 integration preview5298에서 별도로 확인해 가로 넘침과 44px 결함 0건을 확인했다. 업적 Drawer는 닫히고 실제 직원 dialog 하나만 남는지도 검사했다. `verify-integrated-drill.js`와 `integrated-drill-results.txt`를 동봉한다.

기존 브라우저 native confirm(일감 폐기/전체 폐기/회의록 삭제)은 site CSS 대상 React 모달이 아니다. 브라우저 fixture가 confirm 메시지를 수집하고 false로 응답한 12개 경로를 검증했으며 실제 DELETE/POST는 전송하지 않았다. `verify-native-confirms.js`와 `native-confirm-results.txt`에 메시지/취소 결과를 남겼다.

추가 재현은 `verify-deep-records.js`, `deep-results.txt`, `deep-final/`이다. production build, 단위 18개, 격리 production preview5276을 재사용한 E2E9 pass/3 viewport skip을 재확인했다. PNG는 기존48+하위80=128장이며 native confirm12는 시각 캡처 수에 더하지 않는다.
