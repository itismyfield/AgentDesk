export const title = "Herdr 6시간 유휴 자동 종료";
export const state = v => {
  const r = v.rec === "bound" ? "실행 Bound·pane 살아 있음" : "실행 Retired·pane 닫힘";
  const w = [v.turn ? "활성 턴" : "", v.inflight ? "inflight 행" : "", v.dispatch ? "active_dispatch" : ""].filter(Boolean).join("·") || "작업 없음";
  const ph = ({none: "", judged: " · 유휴 판정", fenced: " · 전환 잠금·fence"})[v.phase];
  const s = v.srv === "up" ? (v.registered ? "" : " · runtime 등록 전") : " · 서버 내려감";
  const t = (v.idle ? " · 6시간 유휴" : "") + (v.alerts ? ` · 보류 경보 ${v.alerts}회` : "") + (v.closedBusy ? " · 작업 중 닫힘" : "") + ({none: "", same: " · 같은 provider 세션 재개", fresh: " · 새 세션"})[v.resumed];
  return `${r} · ${w}${ph}${s}${t}`;
};
export const step = a => ({Input: "사람·봇·task-notification 입력", SaveInflight: "inflight 행 저장", DispatchAttach: "dispatch 부착", TurnEnd: "턴 끝", Elapse: "6시간 경과", IdleKillTick: "idle-kill 틱", Fence: "전환 잠금·재확인", Close: "pane 닫기·retire", Crash: "dcserver 재시작", Boot: "부팅", Register: "runtime 등록", Restore: "inflight 턴 복원"})[a] ?? a;
