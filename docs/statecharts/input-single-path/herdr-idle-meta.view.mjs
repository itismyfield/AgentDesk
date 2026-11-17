export const title = "유휴 압축 뒤 입력의 재관측과 전달";
export const state = ({ r }) => ({
  "before-tail": "마지막 대화 기록 보존",
  tail: "비대화 기록이 transcript 끝에 추가됨",
  queued: "owner 입력 도착, 기존 source 보유",
  "busy-ready": "진행 중 턴의 기존 busy 프레임 경로",
  held: r.verdict === "Keep" ? "Unknown 재관측, 입력 영구 보류" : "첫 키 거절, held watch 대기",
  woken: "readiness 증명으로 Ready, owner 깨움",
  "idle-ready": "같은 owner가 Idle 제출 재검증",
  done: r.idlePastes ? "유휴 입력 한 번 전달" : "기존 busy 프레임으로 한 번 전달",
}[r.phase]);
export const step = (action) => ({
  AppendNeutralTail: "첨부·유휴 압축 알림·메타 기록 추가",
  OwnerInput: "사용자 입력을 owner가 받음",
  CheckAdmits: "현재 source와 턴을 첫 키 전에 재독",
  HeldWatch: "공유 판정으로 probe, Keep 또는 Wake",
  OwnerRetry: "깨어난 owner가 Idle 경로 선택",
  IdlePaste: "readiness 재확인 후 붙여넣기",
  BusyFrame: "기존 busy 프레임 전달",
  Done: "전달 완료",
}[action] ?? action);
