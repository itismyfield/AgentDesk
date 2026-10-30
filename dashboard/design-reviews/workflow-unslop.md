# 칸반 디자인 확인

대상: `/kanban`과 카드/백로그 상세, 이슈 할당, 상태 전환/취소 확인, 자동 큐, 파이프라인 편집 표면.

## 기준과 적용

- [Linear 검색 목록](https://linear.app/docs/search)의 이름·상태·식별자 정렬과 제목 대비를 따른다.
- [Vercel 프로젝트 개요](https://vercel.com/blog/dashboard-redesign)의 제목/주요 행동 → 운영 데이터 순서를 사용한다.
- [Grafana Node Exporter](https://grafana.com/grafana/dashboards/1860-node-exporter-full/)의 요약 → 상세 밀도와 구분선 리듬을 참고한다.
- 홈 PR #7562의 작은 헤더, 8px 모서리, 중립 표면과 제한된 상태색을 이어간다. 픽셀 복제나 키트 교체는 하지 않았다.

저장소 선택과 검색/필터를 보드 위의 작은 운영 헤더로 모았다. 담당 필터의 많은 칩은 같은 선택 기능의 콤보박스로 바꿨다. 자동 큐는 신규 브라우저에서 먼저 요약으로 보이며 기존 펼침 저장값은 유지한다. 큐 상세와 모든 기존 동작은 펼쳐서 접근한다. 실데이터의 큐 설명은 펼친 상태에서 유지한다.

PC 보드에서는 lane 최소 폭을 보장해 좁은 PC 화면의 읽기 폭을 확보한다. 모바일은 기존 상태 미니맵과 한 lane 표시를 유지한다. 긴 제목을 생략하지 않고 줄바꿈하며 제목·GitHub·상태 동작에 44px 타깃을 제공한다.

카드·백로그·할당 상세는 공통 Drawer, 담당자/취소 확인은 Modal을 사용한다. Escape, 포커스 복귀, 스크롤과 안전 영역을 공통 경로로 처리한다. 상세의 상태/우선순위/저장소는 작은 텍스트 요약으로 모으고 실제 제목을 먼저 읽게 했다. 파이프라인 편집의 고정 어두운 입력 배경을 테마 표면으로 바꾸고 작은 조작을 44px로 맞췄다. 작은 화면에는 상태·전환 선택 콤보박스를 제공해 작은 그래프 노드만 누르지 않아도 같은 편집기로 진입한다. 그래프 높이는 내용에 맞춰 280–420px 범위로 조절한다.

기능·상태·필터·저장소 추가/제거·저장/실행 동작은 삭제하지 않았다. 검증에서 서버 변경 요청은 브라우저에서 차단했다.

## 캡처와 확인 범위

캡처 루트: `output/playwright/unslop-workflow/`.

- 변경 전: `before/kanban-{390,1440}-{light,dark}.png` (실행 서버 데이터).
- 변경 후: `kanban-final/{390,1440}-{light,dark}-*.png`.
- 개요, 카드 상세, 취소 확인, 담당자 선택, 필터, 고급 필터는 네 조합에서 캡처했다.
- 상세/취소/담당자 상태는 실데이터 카드의 제목·내용에 상태만 브라우저 fixture로 바꿔 재현했다. 실행 서버의 데이터는 변경하지 않았다.
- 백로그 이슈 상세, 이슈 할당, 빈 lane, 파이프라인, 검사기, 전환, PhaseGate, 단계, 에이전트의 백로그 Drawer를 포함해 15개 상태 × 4조합 = 60장을 캡처했다.
- 빈 lane, 할당 대화상자, 편집기 상태 변경은 브라우저 fixture다. 저장/상태 변경 요청은 차단하며 실행 서버에 쓰지 않았다. 개요와 일반 카드 내용은 실행 서버 실데이터다.
- 각 상태에서 정렬·간격·잘림·긴 제목·터치 타깃을 직접 확인했다. Drawer 제목 중복, 라이트 모드 우선순위/자동 큐 대비, 선택 lane 숫자 대비, 작은 파이프라인 조작, 빈 그래프의 과한 높이를 수정하고 재촬영했다.

## 검증 결과

- foundation `5e2edf4b8a` 통합 후 `npm --prefix dashboard run build` 성공.
- 관련 단위 테스트 23개 통과: AutoQueuePanel, PipelineVisualEditorStagePanel, useKanbanBoardModel.
- `PLAYWRIGHT_PORT=5274 npm --prefix dashboard run test:e2e -- --grep 'kanban:|pipeline editor' --reporter=line`: 6개 통과, viewport 전용 2개 skip.
- Playwright CLI 네 검증 스크립트 통과. 390/1440, light/dark 가로 넘침 검사와 파이프라인 버튼·select·checkbox 이외 input의 44px 검사 통과.
- foundation의 preview Origin 수정 통합 후 실제 Vite 프록시에서 WebSocket 연결 표시 정상. 이 묶음은 서버/API 코드를 변경하지 않는다.
- 검증 스크립트: `verify-kanban.js`, `verify-kanban-extra.js`, `verify-pipeline-controls.js`, `verify-backlog-drawer.js` (캡처 archive에 포함).
- 머지·배포는 수행하지 않는다. foundation PR #7564를 부모로 하는 Draft PR이다.
