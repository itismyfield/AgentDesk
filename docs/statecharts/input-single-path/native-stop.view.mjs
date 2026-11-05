export const title = "미기입 source의 live root와 자기 입력 중단 (cbcad30ec3)";
export const state = (v) => {
  if (v.sourceMissing) {
    if (v.result === "sent") return "source 미기입 live 자기 턴 중단 전달";
    if (v.result === "denied") return "source 미기입 live 자기 턴 중단 실패";
    return "live root 확인·durable source 미기입";
  }
  if (v.restoredToken && v.ledgerChild && !v.child) {
    if (v.result === "sent") return "근거 없는 복원 token 중단 전달";
    if (v.result === "denied") return "근거 없는 복원 token 중단 차단";
    return "복원 token: child 근거 없이 ledger만 남음";
  }
  if (v.birthChild !== "unbound") {
    const relation = v.birthChild === "episode" ? "다른 episode" : "다른 parent";
    if (v.result === "sent") return `${relation}에도 중단 전달`;
    if (v.result === "denied") return `${relation}의 중단 차단`;
    return `${relation} child: 복원 거부·원장 확인`;
  }
  if (v.result === "sent") return v.foreign ? "외부 입력에도 중단 전달" : "중단 전달";
  if (v.result === "denied") return (v.attempt === "root" || ["absorbed", "removed"].includes(v.attempt) && v.readable) && !v.foreign ? "자기 입력 중단 실패" : "중단 없음";
  if (!v.opened) return "시작 전";
  if (!v.readable && (!v.relay || ["absorbed", "removed"].includes(v.attempt))) return "원장 확인 불가";
  if (v.foreign) return v.foreignRemoval ? "외부 입력 remove-only 흡수" : "외부 입력 섞임";
  if (v.summary && (v.attempt === "root" || ["absorbed", "removed"].includes(v.attempt))) return "자동요약 뒤 자기 입력";
  if (v.running) return v.pinned ? "실행 분기 자기 root 확인" : "실행 분기 자기 root 미확인";
  if (v.relay) return "릴레이가 맡은 외부 턴";
  if (v.attempt === "removed") return "내 입력 remove-only 흡수";
  if (v.attempt === "absorbed") return "내 입력 Command 확인";
  if (v.attempt === "enqueued") return "자기 입력 대기";
  return "외부 턴 실행";
};
export const step = (action, from, to) => action === "Restart" && to.readable ? null : ({ExternalOpen: "외부 턴 시작", OwnerPaste: "내 입력 붙여넣기", OwnerPasteRunning: "실행 분기에서 내 입력 제출", LiveRootBeforeRuntimeReady: "RuntimeReady 전에 live receipt 확인", Absorb: "내 입력 Command 확인", AbsorbOnly: "내 입력 remove-only 흡수", AbsorbAfterCommand: "같은 입력의 remove 확인", ForeignAbsorbOnly: "외부 입력 remove-only 흡수", CompactSummary: "자동 compact summary", ForeignInput: "다른 입력 유입", RelayClaim: "릴레이가 턴 인계", OtherEpisodeChild: "다른 episode child 복원 거부·원장 확인", OtherParentChild: "다른 parent child 복원 거부·원장 확인", RestoreUnprovenChild: "child 근거 없이 token 복원", Stop: "내 중단 명령", Restart: "재시작 후 원장 재조회", Done: "종료"})[action] ?? action;
