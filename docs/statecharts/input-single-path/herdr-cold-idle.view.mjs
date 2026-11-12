export const title = "pin 없이 공개 Idle 증거로 기존 입력 위임";
export const state = (v) => {
  const r = v.r;
  if (r.phase === "unknownKeep") return "송신 불확실 원문 유지";
  if (r.phase === "discarded") return "clear로 처분한 원문";
  if (r.phase === "held") return r.notice ? "같은 원문 보류 표시" : "같은 원문 보류";
  if (r.phase === "observed" && r.changed) return "관측 뒤 근거 바뀜";
  if (r.phase === "observed" && !r.captured) return "공개 Idle 근거 불충분";
  if (r.phase === "reserved" && r.changed) return "현재 근거와 다른 예약";
  if (r.phase === "releasing" && r.proofVersion !== r.version) return "변한 근거의 거절 누락";
  if (r.phase === "delegate" && r.reserved) return "판독 예약 반환 누락";
  if (r.phase === "idle" && r.reserved) return "예약이 남은 채 Idle 위임";
  return ({ queued: "pin0, 현재 준비 근거 확인", observed: "공개 Idle 증거 관측",
    reserved: "판독 reservation 확보", releasing: "현재성 확인, 반환 대기",
    delegate: "판독 reservation 반환", idle: "기존 Idle driver에 위임",
    sent: "본문과 Enter 기록, 수신 대기", done: "수신 증거로 한 번 정산" })[r.phase];
};
export const step = (action, from, to) => {
  if (action === "Release" && to.r.reserved) return "pre-key 판독 예약 반환 누락";
  if (action === "CheckCurrent" && to.r.proofVersion !== to.r.version && to.r.phase === "releasing") return "변한 근거의 거절 누락";
  return ({
  Observe: "공개 B1 근거 판독", Invalidate: "관측한 source 또는 revision 변경",
  Reserve: "같은 proof 예약 또는 보류", CheckCurrent: "예약의 현재성 다시 확인",
  Release: "pre-key 판독 예약 반환", Delegate: "기존 Idle 경로가 입력을 맡음",
  Notify: "보류 표시", Publish: "같은 대상의 공개 Idle 근거 도착",
  Reobserve: "같은 owner와 새 근거 재확인", Reevaluate: "같은 원문 재평가",
  Discard: "clear가 원문 처분", Write: "기존 driver가 재검증 후 한 번 기록",
  Refuse: "기존 driver가 현재 근거 거절", Receipt: "native 수신 증거 확인",
  Settle: "원문 정산", Wait: "대기"
}[action] ?? action);
};
