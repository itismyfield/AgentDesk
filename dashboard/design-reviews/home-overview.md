# Home 운영 개요 디자인 실험

대상은 `/home` 하나다. `routes.ts`의 기본 진입점이며 사용량·작업·루틴을 함께 보는 화면이라 첫 실험으로 골랐다. 조회 빈도 계측 결과는 아니다. 브랜치와 PR에서 검토하며 머지·배포하지 않는다.

## 기준 화면에서 가져온 원칙

공식 화면 이미지를 실제로 열어 비교했다. 컴포넌트나 픽셀을 복제하지 않았다.

- [Linear 검색 목록](https://linear.app/docs/search): 행의 상태·제목·식별자 축을 맞춘다. 제목의 대비를 높이고 부가 정보는 낮춘다. 항목마다 별도 박스를 만들지 않는다.
- [Vercel 프로젝트 개요 디자인 글](https://vercel.com/blog/dashboard-redesign): 제목·상태·주요 행동을 먼저 배치하고 구분선 아래 운영 내용을 둔다. 과거 공식 화면에서 구조만 참고했다.
- [Grafana Node Exporter](https://grafana.com/grafana/dashboards/1860-node-exporter-full/): 개요 수치에서 상세로 내려가는 순서와 반복 간격, 섹션 간 간격의 차이를 참고했다.

## 적용

- 작은 운영 헤더 → 사용량 요약 → 작업 목록 → 다음 실행순 루틴의 순서다. 확인하지 않은 `2/2 providers connected` 문구를 제거하고 실제 WebSocket 연결만 표시한다.
- 중첩 카드·그라데이션·큰 라운딩을 줄였다. 8px 모서리, 얇은 구분선, 12/16/24px 간격과 명확한 제목 대비를 사용한다. 새 스타일은 Tailwind utility로 표현했다.
- 900px 미만은 토큰/비용 2열, 한도 provider 2열, 작업과 루틴의 세로 읽기 구조다. 900–1023px은 PC shell과 2열 사용량, 1024px 이상은 12열 grid의 3/3/6 요약이다.
- 칸반의 빈 열을 같은 크기로 늘리지 않고 현재 작업과 최근 24시간 완료를 최대 6개 미리 본다. 전체 칸반 링크, 전체 개수, 상태 필터를 제공한다. 정식 보드의 ready/requested·오류 상태 정규화와 QA 라벨을 재사용한다. 취소는 제외하고 백로그·QA 등 현재 작업은 포함한다.
- 긴 제목과 저장소명은 생략하지 않고 줄바꿈한다. 모바일 행은 상태/이슈 → 제목 → 저장소 순서다.
- 기존 저장 순서·편집·드래그·기본값 복원·보조 위젯·루틴 필터·새로고침·상세 drawer를 유지한다. API/WS 변경은 없다.
- 루틴과 한도는 홈 전용 표시를 추가했다. 다른 화면의 기존 표시를 유지한다. 공통 overlay는 닫기 버튼 클래스 전달을 지원하며 홈 루틴 상세에만 44px 타깃을 적용한다. 긴 제목의 줄바꿈도 보장한다.

## 실제 화면 확인 루프

Playwright CLI로 Vite의 실제 React 페이지와 실행 중인 서버의 데이터를 사용했다. 캡처는 KST 2026-10-30에 수행했다. 데이터가 실시간으로 변하므로 전후 수치가 다를 수 있다.

1. 변경 전 390×844 / 1440×1000의 라이트·다크와 루틴 영역을 저장했다.
2. 첫 구현에서 PC 요약의 빈 공간, 테마 전환 중 캡처 문제, QA 상태 필터 누락, freshness의 과도한 live 낭독을 확인했다.
3. 한도 버킷의 라벨/초기화/비율을 한 줄로 모으고 gauge를 줄였다. 테마 전환 완료와 사용량 집계 완료를 기다린 뒤 다시 캡처했다. QA 필터와 연결 문구만 알리는 live 영역을 적용했다.
4. 루틴 상세의 32px 닫기 타깃을 발견해 홈에서 44px로 고친 뒤 다시 확인했다.
5. 최종 4개 기본 화면, 작업·루틴·바닥·상세 화면을 직접 열어 정렬·간격·잘림·긴 문자열·빈 상태·터치 타깃을 확인했다. 독립 리뷰도 수행했다.

검증 범위:

- [x] 390/1440px × light/dark: 겹침·가로 넘침·내부 텍스트 넘침 없음.
- [x] 899/900/1023/1024px 경계: 겹침·가로 넘침 없음.
- [x] 홈의 버튼·링크·select, 홈 루틴 상세 닫기: 최소 44×44px.
- [x] 긴 실데이터: 실제 카드 제목 길이순으로 브라우저 응답을 재배열해 확인. 서버 데이터는 변경하지 않았다.
- [x] 긴 연속 문자열: 별도 브라우저 fixture로 제목/저장소 줄바꿈 확인. 실데이터 캡처와 구분했다.
- [x] 빈 상태: 카드·루틴·한도 응답을 브라우저에서만 비워 확인.
- [x] QA 상태 필터, 루틴 상세 열기/Escape 닫기, 하단 탭바와 마지막 영역 사이 여유 확인.
- [x] 저장/드래그/기본값 복원/보조 위젯과 별도 루틴 페이지 기존 E2E 확인.

환경 한계: 별도 Vite 포트의 WS ticket 요청은 실행 서버의 Origin 검사로 403을 반환한다. 캡처의 연결 끊김 알림은 이 preview 환경 상태다. 연결 성공을 모의하거나 운영 인증 설정을 바꾸지 않았다. 정상 연결 상태는 기존 E2E 및 독립 코드 리뷰로 확인한다. 전역 shell은 이번 디자인 실험의 대상이 아니다.

## 검증 명령과 증거

- `cd dashboard && npm run build`
- `npm run test -- src/components/dashboard/MiniRateLimitBar src/components/dashboard/RoutinesTimelineWidget.test.ts src/components/common/overlay/overlay-primitives` — 22 passed.
- `npm run test:e2e -- --grep 'home:|dashboard: routines timeline|routines: dedicated tab' --reporter=line` — 8 passed, 4 viewport-specific skips.
- `git diff --check`
- Playwright CLI `run-code --filename`으로 `verify-home.js`, `verify-scenarios.js` 실행. 12개 기본/경계 layout 점검에서 실패 0건이며, 별도 12개 긴 데이터/빈 상태 조합도 확인했다.

로컬 캡처 루트는 worktree의 `output/playwright/dash-unslop/`다. PNG와 검증 스크립트는 저장소 소스에 포함하지 않고 Discord 전달 ZIP에 묶는다.

- 전: `before/{390,1440}-{light,dark}.png`, `*-routines.png`
- 후: `final/{390,1440}-{light,dark}.png`, `*-work.png`, `*-filtered.png`, `*-routines.png`, `*-bottom.png`, `390-*-routine-detail.png`
- 시나리오: `scenarios/{long-real,empty,unbroken-stress}-{390,1440}-{light,dark}.png`
- 재현 스크립트: `capture-before.js`, `verify-home.js`, `verify-scenarios.js`
