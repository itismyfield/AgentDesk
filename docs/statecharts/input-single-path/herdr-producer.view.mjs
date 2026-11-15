export const title = "Fresh Claude source의 verified pin 발행";
export const state = (v) => {
  const s = v.s;
  if (!s.registered) return s.earlyHook ? "등록 전 hook은 승격 대기" : "fresh source 등록 전";
  if (!s.hookSeen) return "같은 UUID의 Stat 등록";
  if (!s.verified) return "authentic hook, pin 없음";
  if (s.pinIdentity.ino !== 1) return "잘못된 replacement pin";
  if (s.phase === "done") return "전달 확인";
  if (s.effects === 1) return "본문과 Enter 기록, 수신 대기";
  if (s.phase === "may") return "strict pin 승인";
  return "같은 opened file Verified pin";
};
export const step = (action, from, to) => ({
  RegisterStat: "launch Stat 등록",
  AuthenticHook: "같은 session authentic hook",
  PromoteVerified: "opened fd와 first record 검증 후 pin",
  Drive: to.s.phase === "held" ? "strict pin이 없어 보류" : "strict source 승인 후 입력 시도",
  Effect: "payload와 Enter 한 번",
  NativeProof: "provider 수신 증거",
  Settle: "전달 확정",
  Notify: "보류 표시",
  RepeatVerifiedHook: "반복 hook은 기존 pin 유지",
  Wait: "대기",
  RetryHeld: "pin 발행 뒤 재평가",
}[action] ?? action);
