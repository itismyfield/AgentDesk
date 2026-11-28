export const title = "owner 입력 S1(Claude): busy 분기와 source 선검사(현재) 대 바로 주입, 키 전 등록, 지연 pin 단일 확정, receipt 결합, 흡수 뒤 합류 표시(목표)";
export const state = (v) => {
  const tok = v.token === "none" ? "토큰 없음" : `토큰 ${v.token}`;
  const scr = v.screen === "Idle" ? "화면 Idle" : "화면 Busy";
  const a = v.a === "running" ? "A 실행 중" : v.a === "ended" ? "A 끝남" : "앞 턴 없음";
  const b = {
    absent: "B 도착 전",
    queued: "B 대기열",
    held: "B 보류(키 0)",
    pasted: "B 주입됨(echo 전)",
    joined: "B 진행 중 턴에 합류",
    running: "B 자기 턴 실행",
    ended: "B 끝, 답 게시 전",
    answered: v.merged ? "B 몫이 A 답에 합쳐 게시" : "B 답 게시",
    lost: "B 조용히 유실",
  }[v.b];
  const why = { none: "", blocker: "보류 사유: 보류 조건", source: "보류 사유: source 미결합", busy_unavailable: "보류 사유: busy_source_unavailable" }[v.why];
  const extra = [
    v.trans ? "transcript 있음" : "transcript 없음",
    v.src === "bound" ? "Bound 기록" : "Bound 없음",
    v.reg ? "키 전 등록됨" : "",
    v.pin === "none" ? "" : v.pin === "deferred" ? "지연 pin" : "pin",
    v.sel ? "source 확정" : "",
    v.aBound ? "A anchor 결합" : "",
    v.bBound ? "B anchor 결합" : "",
    why,
    v.blocker === "none" ? "" : `보류 조건 ${v.blocker}`,
    v.mark ? "합류 표시" : "",
    v.aTo === "unset" ? "" : `A 답 reply→${v.aTo}`,
    v.bTo === "unset" ? "" : `B 답 reply→${v.bTo}`,
  ].filter(Boolean).join(", ");
  return `${a} / ${b} / ${tok} / ${scr}${extra ? `, ${extra}` : ""}`;
};
export const step = (action) => ({
  AEnds: "A 종료와 답 게시 (A anchor 결합 뒤, 합류한 B 몫 포함)",
  Arrive: "사람 입력 B 도착",
  DriveCurrent: "owner drive(현재): active turn과 source를 보고 hand_busy 또는 hand_idle",
  DriveTarget: "owner drive(S1): 보류 조건만 보고 키 전 등록 뒤 붙여넣기+Enter",
  RegisterLate: "키 뒤 등록(mutant)",
  BindSource: "writer가 실행의 Bound를 기록",
  Select: "BindingIntent.selected 확정(한 번)",
  BindA: "J2: 꼬리표 없는 opener A를 receipt로 결합",
  BindB: "J2: B를 결합 (흡수면 이때 합류 표시)",
  Retry: "held_watch 재구동",
  TuiAbsorb: "Claude가 도구 경계에서 B를 흡수",
  TuiStart: "TUI가 B를 자기 턴으로 시작 (transcript 파일 생김)",
  BEnds: "B 종료",
  PostB: "B 답 게시 (B anchor reply)",
  ClearBlocker: "보류 조건 해소",
  ResendBeforeEcho: "echo 전 재전송(mutant)",
  TuiDrop: "TUI가 입력 버림(mutant)",
  Done: "끝",
}[action] ?? action);
