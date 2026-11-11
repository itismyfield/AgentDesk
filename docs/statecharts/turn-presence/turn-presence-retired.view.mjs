export const title = "퇴역한 Herdr 채널과 옛 TUI 실행 표식";
const label = v => {
  const h = v.history;
  if (h.removedLive) return "후속 또는 로컬 실행의 표식이 잘못 제거됨";
  if (h.removedForeign) return "다른 실행 종류의 표식이 잘못 제거됨";
  if (h.row === "bound" && h.rowNonce === 2) return "후속 Herdr 실행 중; 옛 정리 요청 대기";
  if (h.stage === "held") return "소유 또는 파일 검증 실패; 생성 보류";
  if (h.started) return "대기 입력이 새 실행을 시작함";
  if (!h.up) return "서버 재시작 중";
  if (!h.reconciled) return "영속 행과 표식을 다시 읽음";
  if (h.row === "retired") {
    if (h.evaluated) return h.activity === "idle" ? "퇴역 채널이 Idle로 확인됨" : "옛 표식 때문에 Unknown 유지";
    return h.settled ? "퇴역 정리 완료; presence 확인 대기" : "퇴역 행 확정; 정확한 표식 정리 대기";
  }
  return ({empty:"TUI 이력 없음",tui:"옛 Codex TUI 표식 존재", "fresh-cleanup":"Herdr Pending; 새 nonce 기록",pending:"Herdr 생성 직전 표식 확인 완료",running:"Herdr 실행 중",ended:"Herdr 종료 증명 완료"})[h.stage] ?? h.stage;
};
export const step = (a, from, to) => {
  if (["FreshMarkerCleanup", "RetiredCleanup"].includes(a)) {
    const removed = (from.history.runtime !== "none" || from.history.legacyRuntime !== "none") && to.history.runtime === "none" && to.history.legacyRuntime === "none";
    return removed ? "정확한 옛 TUI 표식 제거" : "정리 경로가 runtime 표식을 유지";
  }
  return ({TUILaunch:"TUI가 runtime-kind 기록",HerdrFreshLaunch:"새 Herdr nonce와 host 기록",FreshMarkerCleanup:"생성 전 정확한 옛 TUI 표식 정리",HerdrBind:"Herdr 실행 연결",ExecutionEnds:"실행 종료 확인",ConfirmRetired:"행을 Retired로 확정",RetiredCleanup:"퇴역 정리 수행",HistoryRestart:"프로세스 메모리를 버리고 재시작",HistoryBoot:"서버 부팅",RestartReconcileRetired:"퇴역 행 복원 후 정리 재시도",RestartReconcileOther:"영속 상태 복원",NewInput:"새 입력 도착",PresenceEvaluate:"presence가 저장 흔적 판정",StartNewInput:"Idle 입력 시작",ForeignRuntimeMarker:"다른 종류 또는 불가독 표식 도착",ForeignHostMarker:"다른 host 표식 도착",SuccessorNonce:"후속 실행 nonce가 먼저 기록됨",LiveSlotAppears:"같은 이름의 로컬 실행이 나타남",StaleCleanupAttempt:"오래된 정리 요청이 최종 검증",SweepBecomesEligible:"기존 orphan sweep 조건 충족",ExistingOrphanSweep:"기존 orphan sweep가 파일 정리"})[a] ?? a;
};

export const state = v => `${label(v)}${v.history.restarted ? " (재시작 후)" : " (재시작 전)"}`;
