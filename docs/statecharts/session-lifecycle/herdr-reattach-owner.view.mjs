export const title = "재시작 뒤 같은 Herdr 입력의 owner 경로";

const origins = {
  idle: "유휴 pane",
  busy: "작업 중 pane",
  dead: "종료된 실행",
  rowOnly: "실행 행만 확인",
  shellHeld: "foreground 셸과 살아 있거나 불명인 provider",
};

export function state({ attach }) {
  const labels = {
    down: "dcserver 종료",
    await: "첫 입력 대기",
    queued: "같은 ticket과 원본",
    waiting: "원본 보류와 관찰 대기",
    granted: attach.grant === "busy" ? "Busy grant" : "Idle grant",
    reserved: "같은 front의 효과 예약",
    unknown: "Enter 응답 불명, receipt 관찰 중",
    done: "제출 결산",
  };
  const suffix = attach.deadWrite
    ? " (첫 키 전 제어 경계 위반)"
    : attach.submits > 1
      ? " (중복 제출)"
      : attach.ownerControl !== "none" && attach.ownerControl !== "settled"
        ? " (첫 키 전 " + attach.ownerControl + ")"
        : "";
  return [origins[attach.initial], labels[attach.phase] + suffix];
}

const actions = {
  Boot: "dcserver 부팅",
  FirstInput: "원본 수락",
  ReconcileIntake: "intake reconcile 완료",
  ReconnectSource: "부팅 source 관찰",
  OwnerSelect: "현재 증거로 시작 또는 보류",
  OwnerWake: "증거 변화로 같은 원본 깨움",
  OwnerClaim: "동일 front와 followers 예약",
  OwnerCatchUp: "같은 ticket 재수락 시도",
  OwnerInterrupt: "첫 키 전 사망·stop·reset",
  OwnerRejectBeforeKey: "키 0건, 같은 front 보류",
  OwnerSubmit: "provider leaf 1회 실행",
  OwnerReceipt: "제출 증거 결산",
  OwnerRepeatUnknown: "불명 receipt에서 다시 예약",
  AttachDone: "결산 유지",
};

export function step(action) {
  return actions[action] ?? action;
}
