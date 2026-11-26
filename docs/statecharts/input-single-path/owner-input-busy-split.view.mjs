export const title = "owner 입력: busy 분기(현재) 대 바로 주입, 합류 표시, 채널 lease 하나, Codex 항상 Tab, 답 anchor 귀속(목표)";
export const state = (v) => {
  const tok = v.token === "none" ? "토큰 없음" : `토큰 ${v.token}`;
  const scr = v.screen === "Idle" ? "화면 Idle" : "화면 Busy";
  const a = v.a === "running" ? "A 실행 중" : v.a === "ended" ? "A 끝남" : "앞 턴 없음";
  const b = {
    absent: "B 도착 전",
    queued: "B 대기열",
    held: "B 보류(키 0)",
    composing: v.refl ? "B 붙여넣음(화면 반영 확인)" : "B 붙여넣음(반영 전)",
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
    v.aTo === "unset" ? "" : `A 답 reply→${v.aTo}`,
    v.bTo === "unset" ? "" : `B 답 reply→${v.bTo}`,
    v.leases === 0 ? "lease 없음" : v.leases === 1 ? "채널 lease 1" : `채널 lease ${v.leases}(충돌)`,
  ].filter(Boolean).join(", ");
  return `${a} · ${b} · ${tok} · ${scr}${extra ? `, ${extra}` : ""}`;
};
export const step = (action) => ({
  AEnds: "A 종료·답 게시 (합류한 B 몫 포함, terminal 제출 여부는 시나리오)",
  Arrive: "사람 입력 B 도착",
  DriveCurrent: "owner drive: active turn 보고 hand_busy/hand_idle",
  DriveTargetClaude: "owner drive(Claude): 보류 조건만 보고 바로 붙여넣기+Enter",
  PasteCodex: "owner drive(Codex): 보류 조건 확인 뒤 붙여넣기 (바쁨 판정 없음)",
  Reflect: "Codex 입력창에 붙여넣은 frame이 보임",
  KeyCodex: "Codex Tab (idle은 제출, 바쁨은 대기열 수락)",
  Retry: "held_watch·busy retry 재구동",
  TuiAbsorb: "Claude가 도구 경계에서 B를 진행 중 턴에 흡수 (목표: 이때 합류 표시)",
  TuiStart: "TUI가 B를 자기 턴으로 시작, transcript echo",
  BEnds: "B 종료 (마지막 후속 입력: lease 반납)",
  PostB: "B 답 게시(echo 귀속)",
  ClearBlocker: "보류 조건 해소, owner 깨움",
  CapStash: "Idle 상한: 초안 보관 뒤 B 제출",
  ResendBeforeEcho: "echo 전 재전송(mutant)",
  TuiDrop: "TUI가 바쁜 중 받은 입력 버림(mutant)",
  Done: "끝",
}[action] ?? action);
