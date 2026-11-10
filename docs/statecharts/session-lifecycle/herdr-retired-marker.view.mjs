export const title = "퇴역 정리와 Herdr 새 입력 시작";
export const state = v => {
 if (v.q.effects === 1) return "원래 입력이 새 실행에 한 번 전달됨";
 if (v.phase !== "reconciled") return ({running:"퇴역 행과 옛 TUI 표식",down:"서버 재시작", "new-server":"새 Herdr 서버", "herdr-ready":"재시작 상태 복원 대기"})[v.phase] ?? v.phase;
 if (v.row.state === "retired") return v.row.runtimeMarker === "none" ? "표식 정리 완료; 새 입력 시작 가능" : "퇴역 완료지만 옛 TUI 표식으로 시작 보류";
 return ({pending:"새 실행 Pending",placed:"새 pane 생성됨", "fresh-bound":"새 실행 Bound"})[v.row.state] ?? v.row.state;
};
export const step = a => ({DcRestart:"서버 재시작",HerdrReplaces:"Herdr 서버 교체",PaneRootReplaces:"root 교체 관측",Reconcile:"재시작 상태 복원 완료",AcceptInput:"새 입력을 보관",RetiredRuntimeMarkerCleanup:"정확한 퇴역 표식 정리",PrepareFresh:"새 생성 허가 준비",StartLaunch:"presence Idle 후 Pending 예약",CreateOk:"pane 생성",RecordEvidenceAndBind:"실행 근거 기록",SendFresh:"원래 입력 전달"})[a] ?? a;
