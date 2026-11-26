export const title = "owner 입력: busy 분기(현재) 대 바로 주입과 echo 귀속(목표)";
export const state = (v) => {
  const tok = v.token === "none" ? "토큰 없음" : `토큰 ${v.token}`;
  const scr = v.screen === "Idle" ? "화면 Idle" : "화면 Busy";
  const a = v.a === "running" ? "A 실행 중" : v.a === "ended" ? "A 답 게시됨" : "앞 턴 없음";
  const b = {
    absent: "B 도착 전",
    queued: "B 대기열",
    held: "B 보류(키 0)",
    pasted: "B 주입됨",
    running: "B 실행(echo 있음)",
    ended: "B 끝, 답 게시 전",
    answered: "B 답 게시",
    lost: "B 조용히 유실",
  }[v.b];
  const extra = v.blocker === "none" ? "" : `, 보류 조건 ${v.blocker}`;
  return `${a} · ${b} · ${tok} · ${scr}${extra}`;
};
export const step = (action) => ({
  AEnds: "A 종료·답 게시 (terminal 제출 여부는 시나리오)",
  Arrive: "사람 입력 B 도착",
  DriveCurrent: "owner drive: active turn 보고 hand_busy/hand_idle",
  DriveTarget: "owner drive: 보류 조건만 보고 바로 주입",
  Retry: "held_watch·busy retry 재구동",
  TuiStart: "TUI가 B 시작, transcript echo",
  BEnds: "B 종료",
  PostB: "B 답 게시(귀속)",
  ClearBlocker: "보류 조건 해소, owner 깨움",
  CapStash: "Idle 상한: 초안 보관 뒤 B 제출",
  ResendBeforeEcho: "echo 전 재전송(mutant)",
  TuiDrop: "TUI가 바쁜 중 받은 입력 버림(mutant)",
  Done: "끝",
}[action] ?? action);
