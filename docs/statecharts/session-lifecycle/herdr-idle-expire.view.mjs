export const title = "Herdr 6시간 유휴 자동 종료";
export const state = v => {
  const r = (v.prov === "codex" ? "Codex " : "Claude ") + (v.rec === "bound" ? "실행 Bound·pane 살아 있음" : "실행 Retired·pane 닫힘");
  const w = [v.turn ? "활성 턴" : "", v.inflight ? "inflight 행" : "", v.dispatch ? "active_dispatch" : ""].filter(Boolean).join("·") || "작업 없음";
  const ph = ({none: "", judged: " · 유휴 판정", reserved: " · 전환 잠금·행 예약·fence"})[v.phase];
  const s = (v.srv === "up" ? (v.registered ? "" : " · 소유 runtime 없음") : " · 서버 내려감") + (v.gone ? " · 소유 봇 제거됨" : "") + (v.grace ? "" : " · 부팅 유예 중");
  const k = (v.selector === "kept" ? " · 세션 선택자 보존" : "") + ({none: "", effort: " · owner 슬롯 /effort 의도", idle: " · owner 슬롯 유휴 의도", cancelled: " · owner 슬롯 취소됨"})[v.slot] + (v.file ? " · 유휴 재개 기록" : "") + ({none: "", recorded: " · /effort 종료 대기", stale: " · /effort 대상 바뀜"})[v.effort];
  const t = (v.idle ? " · 6시간 유휴" : "") + (v.alerts ? ` · 보류 경보 ${v.alerts}회` : "") + (v.closedBusy ? " · 작업 중 닫힘" : "") + (v.closedUsed ? " · 다시 쓴 pane 닫힘" : "") + ({none: "", same: " · 같은 provider 세션 재개", fresh: " · 새 세션"})[v.resumed] + (v.cleared ? " · /clear 됨" : "") + ({none: "", alone: " · /clear 재개 금지 기록됨(단독)", over_start: " · /clear 재개 금지 기록됨(시작 겹침)", held: " · /clear 재개 금지 기록됨(Held)", failed: " · /clear 선택자 쓰기 실패"})[v.clearing] + (v.clearResumed ? " · clear 뒤 옛 대화 재개" : "");
  return `${r} · ${w}${ph}${s}${k}${t}`;
};
export const step = a => ({Input: "소유 봇 입력", SaveInflight: "inflight 행 저장", DispatchAttach: "dispatch 부착", TurnEnd: "턴 끝", Elapse: "6시간 경과", EffortStart: "/effort 의도 저장", EffortEnd: "/effort 종료", IdleKillTick: "idle-kill 틱", Reserve: "전환 잠금·행 예약·재확인·재개 기록", Close: "pane 닫기·retire", Crash: "dcserver 재시작", Boot: "부팅", GracePass: "부팅 유예 지남", Register: "runtime 등록", Return: "제거된 봇 복귀", Restore: "inflight 턴 복원", ClearBan: "/clear 재개 금지 기록", ClearSelector: "/clear SQL 선택자 갱신"})[a] ?? a;
