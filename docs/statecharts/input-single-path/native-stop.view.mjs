export const title = "외부 Claude 턴에 흡수된 자기 입력의 중단 (216858a711)";
export const state = (v) => {
  if (v.result === "sent") return "중단 전달";
  if (v.result === "denied") return (v.attempt === "root" || v.attempt === "absorbed" && v.readable) && !v.foreign ? "자기 입력 중단 실패" : "중단 없음";
  if (!v.opened) return "시작 전";
  if (v.running) return v.foreign ? "외부 입력 섞임" : v.pinned ? "실행 분기 자기 root 확인" : "실행 분기 자기 root 미확인";
  if (!v.readable && (!v.relay || v.attempt === "absorbed")) return "원장 확인 불가";
  if (v.foreign) return "외부 입력 섞임";
  if (v.relay) return "릴레이가 맡은 외부 턴";
  if (v.attempt === "absorbed") return "자기 입력 흡수";
  if (v.attempt === "enqueued") return "자기 입력 대기";
  return "외부 턴 실행";
};
export const step = (action, from, to) => action === "Restart" && to.readable ? null : ({ExternalOpen: "외부 턴 시작", OwnerPaste: "내 입력 붙여넣기", OwnerPasteRunning: "실행 분기에서 내 입력 제출", Absorb: "Claude가 내 입력 흡수", ForeignInput: "다른 입력 유입", RelayClaim: "릴레이가 턴 인계", Stop: "내 중단 명령", Restart: "재시작 후 원장 재조회", Done: "종료"})[action] ?? action;
