export const title = "Herdr 실행 퇴역 뒤 세션 행 status";
export const state = v => {
  const e = ({bound: "실행 Bound", retired: "실행 Retired", pending: "새 실행 Pending"})[v.exec];
  const r = ({turn_active: "행 turn_active", disconnected: "행 disconnected", idle: "행 idle"})[v.row];
  const n = v.nonce === 1 ? "퇴역 턴" : "새 턴";
  const b = ({alive: "reader 살아 있음", exiting: "reader 종료 중", final: "reader 마무리", gone: "reader 없음"})[v.bridge];
  const p = ({busy: "presence busy", unknown: "presence unknown", idle: "presence idle"})[v.pres];
  const ph = ({run: "턴 진행", postlude: "force postlude 전", liveend: "row end 정리 전", settle: "terminate settle 전", done: "취소·row end 끝"})[v.phase];
  const t = (v.ended ? " · 한때 끝남" : "") + (v.refused ? " · 새 턴 거절됨" : "") + (v.turns ? " · 새 턴 시작 뒤" : "") + (v.restarts ? " · 재시작 뒤" : "") + (v.lost ? " · reader 행 상실" : "") + (v.lostWrite ? " · 상실 뒤 씀" : "") + (v.oldOverNew ? " · 새 턴 행 덮음" : "");
  const sh = ({pathless: "경로 없는 행", path: "output_path 행", kind: "terminal kind 행", receipt: "before-start receipt 행"})[v.shape];
  const o = v.inf === "present" ? ` · inflight ${sh}(${v.igen === "prior" ? "이전 세대" : "현 세대"}${v.boot === "pending" ? ", 부팅 복구 전" : ""}${v.committed ? ", 공지 ack" : ""})` : (v.gone === "no" ? "" : ` · inflight ${v.gone === "dropped" ? "나이로 삭제" : "정리됨"}`);
  return `${ph} · ${e} · ${r}(${n}) · ${b} · ${p}${v.fresh ? "" : " · heartbeat 오래됨"}${t}${o}${v.notices ? ` · 공지 ${v.notices}회` : ""}`;
};
export const step = a => ({Retire: "terminate·row end 퇴역", ForcePostlude: "force postlude disconnect", LiveEnd: "live row end 정리", Heartbeat: "reader 30초 working", BridgeExit: "루프 끝 TURN_ACTIVE", BridgeFinal: "terminal IDLE(또는 생략)", StaleTick: "300초 경과", Sweep: "stale sweep idle", Install: "새 실행 Pending 설치", NewTurn: "새 nonce 턴 시작", NewTurnEnd: "새 턴 IDLE", Restart: "dcserver 재시작", PresenceStale: "600초 정체 → unknown", TermSettle: "terminate settle_captured", BootRestore: "부팅 복구 1회", EndPass: "row end pass 정리", StaleDrop: "stale-age 삭제"})[a] ?? a;
