export const title = "Stop continuation 출력 뒤 늦게 도착하는 opener";
export const state = ({ r }) => {
  if (r.idlePastes > 0 && r.continuingStop && !r.continuationComplete)
    return "미완료 continuation 위에 Idle 붙여넣기";
  if (r.idleVerified && r.continuingStop && !r.continuationComplete)
    return r.openerVisible ? "후속 턴 미완료인데 Idle 증명" : "후속 opener 전인데 Idle 증명";
  return ({
    "before-tail": "끝답변 end_turn 기록",
    tail: r.continuingStop ? "Stop 성공 출력이 continuation을 요청" : "quiet Stop 성공 출력",
    queued: "같은 owner front의 입력 도착",
    "busy-ready": "후속 Busy 턴의 기존 프레임 경로",
    held: r.verdict === "Keep" ? "continuation 완료까지 Keep" : "Idle 제출 거절, watch 보존",
    woken: "공유 probe Ready, owner 깨움",
    "idle-ready": "후속 완료 증명으로 Idle 제출 준비",
    done: r.idlePastes ? "Idle 입력 한 번 전달" : "기존 Busy 프레임 한 번 전달",
  })[r.phase];
};
export const step = (action) => ({
  AppendStopOutput: "Stop hook_success 기록, stdout에 continuation 요청",
  ContinuationOpener: "뒤늦은 additional_context와 새 opener 기록",
  ContinuationFinishes: "후속 턴의 새 완료 기록",
  OwnerInput: "owner 입력 도착",
  CheckAdmits: "현재 source와 턴 재독",
  HeldWatch: "같은 probe로 Keep 또는 Wake",
  OwnerRetry: "같은 owner 재개",
  IdlePaste: "첫 키 전 재검증 후 Idle 붙여넣기",
  BusyFrame: "기존 Busy 프레임 전달",
  RevalidateIdle: "그 사이 변한 source를 첫 키 전에 거절",
  RevalidateBusy: "그 사이 변한 source를 첫 키 전에 거절",
  Done: "완료",
}[action] ?? action);
