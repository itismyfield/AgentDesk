export const title = "재시도: 잠금 뒤 관측하고 같은 transaction에서 적용";
export const state = ({s}) => {
  const phase = {
    begin: "시작 전", "early-observe": "잠금 전 오래된 관측", source: "원본 행 잠금 대기",
    budget: "원본 잠김·예산 확인", owner: "소유자 잠금 대기", worker: "worker 잠금 대기",
    "early-end": "관측 전에 transaction 종료", parse: "모든 잠금 유지·요구 파싱",
    profile: "모든 잠금 뒤 인증 관측", readiness: "forwarding·시각 관측", apply: "관측한 후보로 적용",
    commit: "결과 commit", rollback: "파싱 오류 rollback", done: "종료"
  }[s.pc];
  if (s.applied && (!s.appliedUnderLocks || s.appliedTx !== 1)) return "다른 transaction으로 적용";
  if (s.applied && (s.profileVersionRead < s.lockProfileVersion || s.forwardingVersionRead < s.lockForwardingVersion)) return "잠금 전 관측으로 잘못 적용";
  if (["parse", "profile", "readiness", "apply"].includes(s.pc) && !s.sourceLock) return `잠금을 놓친 뒤: ${phase.replace("모든 잠금 유지·", "").replace("모든 잠금 뒤 ", "")}`;
  return phase;
};
export const group = ({s}) => s.pc === "done" ? "완료" : s.pc === "early-observe" || s.pc === "early-end" ? "결함변형" : "재시도";
export const colors = { "완료": "teal", "결함변형": "pink", "재시도": "blue" };
export const step = (action, from) => {
  if (from.s.sampledBeforeLock && ["ObserveAuthProfile", "ObserveForwardingAndTime"].includes(action)) return "잠금 전 관측 재사용";
  return ({
  Begin: "transaction 시작", ObserveBeforeLocks: "잠금 전에 설정과 cache를 읽음", LockSource: "원본 행 잠금",
  CheckFamilyBudget: "family 예산 확인", LockOwner: "소유자 잠금 또는 기존 비소유 경로", LockWorker: "worker 공유 잠금",
  EndTransactionEarly: "원래 transaction을 먼저 끝냄", ParseRequirements: "실행 요구 파싱", ObserveAuthProfile: "현재 인증 profile 관측",
  ObserveForwardingAndTime: "현재 forwarding·TTL 관측", Apply: "선택 후보 INSERT 또는 no-target 갱신",
  Commit: "같은 transaction commit", Rollback: "오류 rollback", ChangeAuthProfile: "profile 변경", ExpireForwarding: "forwarding TTL 만료", Done: "완료"
}[action] ?? action);
};
