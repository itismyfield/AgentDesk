export const title = "Codex canary Herdr 퇴역 뒤 새 실행 시작";
export const state = v => {
  if (v.pub === "old") return "옛 tmux source 게시(경계 밖)";
  const w = ({booting: "writer 부팅", blocked: "writer 막힘(옛 pin)", resumed: "writer 준비됨"})[v.writer];
  const r = ({retired: v.cleanup === "done" ? "퇴역 정리 완료" : "퇴역 정리 중", pending: "새 실행 Pending", bound: "새 실행 Bound"})[v.row];
  const m = v.marker === "n2" ? "새 nonce 표식" : "표식 없음";
  const l = ({none: "listing 없음", rw: "listing 퇴역 nonce(withheld)", ra: "listing admitted"})[v.listing];
  const i = ({none: "입력 없음", queued: "front 판정 대기", held: v.watch ? "front 보관(관측 중)" : "front 보관(관측 없음)", gateway: "입력 통과", admitted: "턴 호스트 통과", cas: "Pending CAS", launched: "새 실행 시작 중", done: "턴 시작됨", failed: "시작 실패", cancelled: "취소"})[v.intake];
  const t = (v.boots ? " · 재시작 뒤" : "") + (v.fails ? " · 실패 1회 뒤" : "") + (v.tmuxLive ? " · tmux 살아 있음" : "");
  return `${w} · ${r} · ${m} · ${l} · ${i}${t}`;
};
export const step = a => ({NewInput: "입력 도착", NextInput: "다음 입력 차례", Boot: "부팅(first_drain까지)", Resume: "source 허용 → resume", Drive: "owner front 판정", TurnHost: "턴 호스트 판정", ReadyWake: "writer 준비 wake", WatchWake: "bootstrap 관측 wake", LaunchCAS: "Retired→Pending CAS·listing", LaunchRefused: "빈자리 재확인 실패", MarkerOk: "새 nonce 표식 게시", MarkerFail: "표식 쓰기 실패 → 퇴역", AdmitPending: "Pending 재사용", Followup: "Bound 후속 입력", Bind: "첫 입력·Bound·admit", LaunchNotSent: "생성 미전송 → 퇴역", Unplaced: "pane 없는 Pending 회수", Publish: "새 실행 source 게시", PublishOld: "옛 tmux source 게시", Retire: "Terminate·clear·stop", CleanupDone: "퇴역 정리 완료", MarkerLost: "이름 기준 정리로 표식 삭제", TmuxEnds: "tmux 종료", TmuxAppears: "tmux 출현", Restart: "dcserver 재시작", Cancel: "취소"})[a] ?? a;
