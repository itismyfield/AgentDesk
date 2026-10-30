# 전체 언슬롭: 공통 셸과 조작면

홈 실험 PR #7562의 후속이며 이 브랜치는 홈 브랜치를 base로 쌓는다. Draft PR 검토까지만 진행하고 머지·배포하지 않는다.

기준은 [Linear 검색 목록](https://linear.app/docs/search)의 정렬과 절제된 대비, [Vercel 프로젝트 개요](https://vercel.com/blog/dashboard-redesign)의 제목/주요 조작 구획, [Grafana](https://grafana.com/grafana/dashboards/1860-node-exporter-full/)의 요약→상세와 간격 리듬이다. 기존 공식 화면 조사와 승인된 홈의 구조를 확장하며 픽셀은 복제하지 않는다.

## 변경과 영향

- PC 900px 이상: 216px 메뉴, 작은 아이콘과 44px 행, 정렬된 검색/조작 헤더. WS 연결을 provider 수로 잘못 표현하던 사이드바 문구 수정.
- 모바일 900px 미만: breadcrumb 전용 행/44px 검색·아이콘, 6개 하단 탭과 모든 확장 메뉴 유지. safe-area bottom/left/right 보존. 확장 메뉴를 구분선 목록으로 바꾸고 Tailwind로 옮겨 별도 CSS 제거.
- 재연결 알림은 아이콘 위의 fixed overlay에서 헤더의 별도 줄로 이동. 390/1440 × light/dark에서 알림/조작의 교차 면적 0 확인. 정상 연결 때는 이 줄이 숨겨진다.
- 알림·디자인 패널, 명령 팔레트와 빈 검색, 단축키 창, 공통 Modal/Drawer/BottomSheet의 테마·스크롤·제목 줄바꿈·44px 닫기를 정리했다. 단축키는 공통 Modal의 포커스 복귀와 Escape를 사용한다.
- SurfacePrimitives의 제목/보조문구/수치 위계, 중립 표면, 간격과 44px 동작을 통일했다. 개별 화면의 IA/표시는 후속 묶음에서 검증한다.
- 실제 데이터의 null 지역명 때문에 검색을 여는 순간 cmdk `.trim()`에서 앱이 사라지는 문제를 발견했다. 라이브러리에 넘기는 키워드를 한 번 정리하고 기존 검색 E2E에 null fixture를 추가했다.
- 병렬 화면 검증이 서로 다른 dist를 재사용하지 않도록 기존 Playwright 설정에 `PLAYWRIGHT_PORT`를 추가했다. 기본값4173은 유지한다.

## WebSocket 403 진단

Vite의 문자열 `/api` proxy는 changeOrigin=true로 정규화되어 Host만8791로 바꿨다. Origin은 preview 포트이므로 backend의 정확한 Origin/Host 검사를 통과하지 못했다. `/api`와 `/ws` 모두 `changeOrigin:false`를 명시해 public Host를 보존했다. 서버 인증·Origin 정책은 바꾸지 않았다.

확인 결과(티켓/자격증명은 출력하지 않았다):

- 실제 release localhost8791: 티켓200 → WS101 → connected.
- 실제 release Tailscale IP8791: 200 → 101 → connected.
- 실제 release MagicDNS8791: 200 → 101 → connected.
- preview Origin/재작성 Host: 403 재현.
- 수정한 실제 Vite5188: 티켓200. production build preview5288의 실제 React 페이지도 WS연결을 확인했다.
- 외부 TLS proxy를 추가한다면 public Host 보존이 필요하다. 현재 직접/Tailscale 배포에는 같은403이 재현되지 않았다.

## 검증

- `npm run build` 성공.
- 기존 WS/overlay 단위시험11개 통과.
- `PLAYWRIGHT_PORT=5273 npm run test:e2e -- --grep 'responsive:|desktop:|command palette|navigation:|theme|home:' --reporter=line`: 15 passed, viewport별11개 정상skip.
- 실제 production preview에서 390×844/1440×1000 × light/dark: 정상 헤더, 빈 알림, 디자인 패널, 검색, 빈 검색, 단축키. 모바일 확장 메뉴 추가 확인.
- 별도 browser WS 차단 fixture에서 네 조합의 재연결 헤더 캡처. 실제 운영 서버 상태/설정 변경 없음.
- 각 캡처에서 조작44px·가로 넘침·메뉴/본문 정렬·긴 데이터·오버레이 스크롤을 자동 계측하고 직접 이미지를 열어 확인했다. 알림 popover의 PC 오른쪽 넘침, 단축키 높이, 검색 null crash를 고친 뒤 다시 찍었다.

스크린샷/재현 스크립트: worktree의 `output/playwright/unslop-foundation/{before,after}/`, `verify.js`. 이 자료는 Discord ZIP으로 전달한다. 홈 사용량 집계가 느린 시점의 `…`는 측정0을 뜻하지 않는 기존 로딩 상태다.

### 상태 배지 대비 재검증

라이트에서 파스텔 고정색을 쓰던 공통 상태 토큰을 테마 배경/문구 색으로 전환했다. 경고 문구 혼합 비율도 조정했다. 실제 CSS를 읽는 Playwright 팔레트 fixture(일반 화면 데이터 아님)에서 건강/칸반 19종 × 2테마의 대비를 계산해 최저 4.74:1을 확인했다. `after/status-palette-{light,dark}.png`에 결과를 남겼으며 StatusBadge/Freshness 관련 unit 16건과 production build가 통과했다.

### 작은 조작·화면 경계 확인

후속 오류 화면에서 Sonner 닫기가 20px인 점을 발견해 44px로 늘리고 문구 영역과 분리했다. 긴 오류 문구로 4조합 모두 버튼 44×44, 문구 교차 면적 0을 확인했다(`after/toast-*.png`). 공통 TooltipLabel도 키보드 동작이 없는 span을 native 버튼으로 바꾸고, 44px 조작·12px 도움말·화면 좌우 경계 보정을 적용했다. production preview에서 Enter로 열기/Escape로 닫기와 4조합 경계 내 배치를 확인했다(`after/tooltip-*.png`).

추가 에이전트 폼 검수에서 배경 API 오류 토스트가 모달 하단 조작을 가로막는 점도 확인했다. Toaster를 공통 Modal/Drawer의70·80보다 낮은65층에 두어 배경 알림이 모달 조작을 가로채지 않게 했다. 4조합 실제 브라우저에서 교차 영역의 hit-test와 일반 닫기를 통과했다(`after/toast-modal-*.png`).
