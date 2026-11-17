export const title = "동시에 온 입력은 같은 스레드 세션으로 합류";
export const state = (v) => {
  const pcs = Object.values(v.phase);
  const count = p => pcs.filter(x => x === p).length;
  const host = v.session === "booting"
    ? Object.values(v.delivered).some(x => x > 0) ? "살아 있는 세션 재시작" : "첫 세션 시작"
    : v.session === "live" ? "살아 있는 세션" : "세션 없음";
  return `${host} · 보관${count("queued")} 확인${count("peeked")} 준비${count("preparing")} 시작${count("starting")} 전달${count("submitted")} 답${count("done")} 실패${count("failed")}`;
};
export const step = (action) => ({
 Peek: "세션 존재 관측", TakeTransition: "같은 스레드 전환 확보 후 재확인",
 Start: "첫 메시지 경로 선택", StartupReady: "첫 입력 전달과 세션 준비",
 Warm: "기존 세션에 다음 입력 전달", Failure: "해당 입력 실패 알림",
 Post: "해당 입력의 답 게시", Done: "완료 유지"
}[action] ?? action);
