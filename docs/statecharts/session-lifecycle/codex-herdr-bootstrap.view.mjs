export const title = "Codex canary Herdr 퇴역 뒤 새 실행 시작";
export const state = v => {
  if (v.pub === "old") return "옛 tmux source 게시(경계 밖)";
  if (v.writer === "booting") return "writer 부팅" + (v.boots ? " · 재시작 뒤" : "");
  const w = v.writer === "resumed" ? "writer 준비됨" : "writer 막힘(옛 pin)";
  const r = ({retired: v.cleanup === "done" ? "퇴역 정리 완료" : "퇴역 정리 중", pending: "새 실행 Pending", bound: "새 실행 Bound"})[v.row];
  const i = ({none: "입력 없음", held: "입력 보관", gateway: "입력 통과", admitted: "턴 호스트 통과", launched: "새 실행 시작 중", done: "턴 시작됨", cancelled: "취소"})[v.intake];
  const m = v.marker === "n2" ? "새 nonce 표식" : "표식 없음";
  const t = (v.boots ? " · 재시작 뒤" : "") + (v.fails ? " · 생성 실패 1회 뒤" : "") + (v.tmuxLive ? " · 옛 tmux 살아 있음" : "");
  return `${w} · ${r} · ${m} · ${i}${t}`;
};
export const step = a => ({Boot: "부팅(first_drain까지)", Resume: "source 허용 → resume", InputArrives: "입력 도착", Recheck: "보관 입력 재판정", TurnHost: "턴 호스트 판정", Launch: "Retired→Pending CAS·새 nonce 표식", LaunchRefused: "빈자리 재확인 실패", AdmitPending: "Pending 재사용", Followup: "Bound 후속 입력", Bind: "첫 입력·Bound", LaunchNotSent: "생성 미전송 → 퇴역", Unplaced: "pane 없는 Pending 회수", Publish: "새 실행 source 게시", PublishOld: "옛 tmux source 게시", Retire: "Terminate·clear·stop", CleanupDone: "퇴역 정리 완료", MarkerLost: "이름 기준 정리로 표식 삭제", TmuxEnds: "옛 tmux 종료", Restart: "dcserver 재시작", Cancel: "취소"})[a] ?? a;
