# 운영 화면 디자인 검토

홈 #7562와 foundation #7564의 정보 위계를 운영 화면에 확장한다. Draft PR까지만 만들고 머지·배포하지 않는다.

기준: [Linear 검색 목록](https://linear.app/docs/search)의 일관된 행 정렬, [Vercel 프로젝트 개요](https://vercel.com/blog/dashboard-redesign)의 상태→핵심 조작 위계, [Grafana Node Exporter](https://grafana.com/grafana/dashboards/1860-node-exporter-full/)의 요약→진단 밀도를 해석한다. 픽셀은 복제하지 않는다.

## 작업 전 화면·상태 목록

- 운영 요약: health 정상/주의/장애, 응답 대기/실패/마지막 정상값, 실시간 연결·STALE·업데이트·복구 상태.
- 주요 지표: 후처리 대기, outbox 지연, 처리 대기열, 감시 수. 임계값과 색상 의미 유지.
- 운영 시그널 6개: 연결·큐·전달·프로바이더·감시·복구.
- Runtime watchlist: 병목 목록, 긴 reason/detail, 병목 없음, health 응답 대기.
- 연결과 전달: WebSocket, dispatch outbox, providers, prompt retention 정상/꺼짐/요청 실패/긴 config_source.
- 실행 장비: 단독 운영/빈 cluster/노드 API 실패·오래된 데이터, 장치 연결·준비·용량·진단, 긴 hostname/instance/provider/backend.
- 세션: 출력 보기/닫기, 출력 대기/실패/미지원/긴 출력, 중지 확인/취소. 실제 실행 중지는 하지 않는다.
- 기존 회의 타임라인 대기 위젯과 관리 화면의 에이전트·부서 링크 유지.

현재 source에 NodeConnect 화면/route가 없어 이 묶음 범위에서 제외한다. 부모가 범위를 확인했다.

## 변경 의도

현재 상태와 주요 지표 → 확인이 필요한 병목 → 운영 시그널/연결·보관 → 실행 장비·세션 → 회의와 전용 관리 경로 순서로 배치한다. 장비 상세 때문에 현재 상태 요약이 모바일 첫 화면 밖으로 밀리던 문제를 해결한다.

지표는 중첩 카드 대신 정렬된 요약 셀, 시그널은 label/value/detail 행, watchlist는 구분선 목록을 쓴다. 연결 패널의 중첩 mini card도 제거했다. 모바일은 요약 2열과 본문 1열, PC는 요약 4열과 진단 2열이다. 관리 경로에 기존 에이전트·부서와 머신/런타임 설정 링크를 둔다.

OpsPageModel의 293줄 style 문자열을 제거하고 Tailwind utility로 옮긴다. API 요청·주기·시그널 계산·임계값·제어 콜백과 중지 확인을 유지한다. 응답 전에는 0을 정상 값처럼 강조하지 않고 대시와 대기 안내를 사용한다.

## 검증 및 캡처

`output/playwright/unslop-ops/`에 기준/최종/fixture 캡처와 재현 스크립트를 둔다. API 문서는 `/api/docs`, `/api/docs/observability/health`, `/api/docs/observability/cluster`, `/api/docs/internal/ops`를 먼저 확인했다.

390×844 / 1440×1000 각각 light/dark에서 실제 페이지를 찍고 overflow·정렬·간격·하단 가림·44px 타깃을 직접 확인한다. 긴 데이터·빈 상태·오류·클러스터 세션 제어는 브라우저 fixture를 사용한다. 브라우저의 실제 쓰기 요청을 차단하고 설정 저장·세션 중지 등 운영 상태는 바꾸지 않는다.

실화면 검사에서 긴 degraded reason이 주요 수치를 밀어내는 문제를 발견해, 진단 문자열을 지표 뒤로 옮겼다. 긴 hostname/session/backend/output/config_source는 줄바꿈하며 세션 출력은 별도 경계로 구분한다.

- build 성공 (foundation 4c299336f0 기반)
- 관련 HealthWidget/StatusBadge unit 12개 통과
- 기존 Ops E2E 6개 통과 (상태/병목, WS 이벤트 갱신, 출력·중지 fixture, stale 제어 차단)
- 기본 화면 28개 + 상태 fixture 60개 =88회 overflow/44px 검사 결함 0건
- 기준 4장 + 최종 28장 + 상태 60장 =92개 PNG. `before/`, `final/`, `states/`
- `capture-baseline.js`, `capture-final.js`, `capture-states.js` 재현 스크립트. Playwright CLI session `unslop-ops`, Vite5295, 기존 E2E5279로 격리.
- `git diff --check`, stacked delta cap 4 production files / net -557 통과.

부모의 독립 검토 후에만 완료 알림을 보낸다. API/WS/schema와 운영 설정은 변경하지 않았다.
