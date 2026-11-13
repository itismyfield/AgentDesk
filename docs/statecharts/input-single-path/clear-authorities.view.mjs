export const title = "공개 준비 증거와 clear 채택 증거";
export const state = ({ r }) => {
  const queue = Object.values(r.q ?? {});
  const wrote = Object.values(r.writes ?? {}).some((n) => n > 0);
  const terminal = !r.continuing && (r.tail === "hard" || (r.tail === "summary" && r.stopBoundary === r.turn));
  const ready = r.observed === "Idle" && r.seenEpoch === r.epoch && r.seenTurn === r.turn
    && (!r.needsStart || r.started) && !r.continuing && !r.compactFence
    && ((r.started && r.turn === 0) || r.compactReady || (r.terminal === r.turn && r.turn > 0 && terminal));
  if (wrote && !r.adopted && !r.oldBinding && ready) return "채택 없이 전달 후 공개 준비와 binding 변경";
  if (wrote && !r.adopted && !r.oldBinding) return "채택 없이 전달 후 binding 변경";
  if (wrote && !r.adopted) return "채택 없이 본문과 Enter 전달";
  if (queue.every((q) => q === "done")) return "전달 확인";
  if (wrote) return "본문과 Enter 전달, 확인 대기";
  if (r.adopted) return "clear 채택 완료, 첫 키 전";
  if (r.adoptionRefused && r.diskCurrent) return "인증서 회복, 채택 재검사";
  if (r.adoptionRefused) return "채택 거절, 원본 보류";
  if (r.phase === "Booting") return "현재 실행의 시작 증거 대기";
  if (r.observed === "Idle" && r.started) return "공개 준비 확인, 채택 대기";
  return "공개 준비 증거 관측";
};
export const group = ({ r }) => Object.values(r.writes ?? {}).some((n) => n > 0) && !r.adopted
  ? "unsafe" : r.adoptionRefused && !r.diskCurrent ? "held" : "normal";
export const colors = { unsafe: "pink", held: "yellow", normal: "teal" };
export const step = (action) => ({
  SessionStart: "인증된 시작 기록", Observe: "현재 ledger 준비 확인",
  Hold: "원본 보류", Prepare: "현재 입력 준비", AdoptAwaitedClear: "유효한 디스크 증명으로 채택",
  RefuseAwaitedClear: "디스크 증명 거절", RepairClearCertificate: "현재 디스크 증명 회복",
  Submit: "본문과 Enter 전달", Finish: "프로바이더 턴 종료", Receipt: "제출 영수증 확인",
  OldStop: "이전 종료 알림", Stop: "현재 종료 알림", IdleWake: "기존 입력 다시 확인",
  PublishClearFile: "새 transcript 게시"
}[action] ?? "입력 상태 재확인");
