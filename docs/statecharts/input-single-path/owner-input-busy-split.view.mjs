export const title = "owner 입력: busy 분기(현재) 대 바로 주입, 합류, 키 선택(목표)";
export const state = (v) => {
  const tok = v.token === "none" ? "토큰 없음" : `토큰 ${v.token}`;
  const scr = v.screen === "Idle" ? "화면 Idle" : "화면 Busy";
  const a = v.a === "running" ? "A 실행 중" : v.a === "ended" ? "A 답 게시됨" : "앞 턴 없음";
  const b = {
    absent: "B 도착 전",
    queued: "B 대기열",
    held: "B 보류(키 0)",
    keying: `B 키 고르는 중(관측 ${v.seen})`,
    pasted: "B 주입됨(echo 전)",
    joined: "B 진행 중 턴에 합류",
    running: "B 자기 턴 실행(echo 있음)",
    ended: "B 끝, 답 게시 전",
    answered: v.merged ? "B 몫이 A 답에 합쳐 게시" : "B 답 게시",
    stuck: "B 입력창·대기열에 걸림",
    lost: "B 조용히 유실",
  }[v.b];
  const extra = [
    v.blocker === "none" ? "" : `보류 조건 ${v.blocker}`,
    v.mark ? "합류 표시" : "",
    v.aLost ? "A 작업 유실" : "",
  ].filter(Boolean).join(", ");
  return `${a} · ${b} · ${tok} · ${scr}${extra ? `, ${extra}` : ""}`;
};
export const step = (action) => ({
  AEnds: "A 종료·답 게시 (합류한 B 몫 포함, terminal 제출 여부는 시나리오)",
  Arrive: "사람 입력 B 도착",
  DriveCurrent: "owner drive: active turn 보고 hand_busy/hand_idle",
  DriveTargetClaude: "owner drive(Claude): 보류 조건만 보고 바로 붙여넣기+Enter",
  ObserveCodex: "owner drive(Codex): 보류 조건 확인 뒤 화면 한 번 읽어 키 고름",
  KeyCodex: "Codex 키 입력 (바쁨 Tab · idle Enter · 불확실 규칙)",
  Retry: "held_watch·busy retry 재구동",
  TuiAbsorb: "Claude가 도구 경계에서 B를 진행 중 턴에 흡수",
  TuiStart: "TUI가 B를 자기 턴으로 시작, transcript echo",
  BEnds: "B 종료",
  PostB: "B 답 게시(echo 귀속)",
  ClearBlocker: "보류 조건 해소, owner 깨움",
  CapStash: "Idle 상한: 초안 보관 뒤 B 제출",
  ResendBeforeEcho: "echo 전 재전송(mutant)",
  TuiDrop: "TUI가 바쁜 중 받은 입력 버림(mutant)",
  Done: "끝",
}[action] ?? action);
