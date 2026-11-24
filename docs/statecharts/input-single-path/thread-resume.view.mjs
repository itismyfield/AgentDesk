export const title = "검증 바인딩 없는 Claude 스레드 cold resume (#8372)";
const fate = { absent: "미도착", queued: "보관 대기", submitted: "세션에 입력", answered: "답 게시",
  retained: "원문 보관(다시 보내라는 알림)", uncertain: "재시작으로 불확실(실패 알림 대기)",
  failed: "실패 종결" };
export const state = (v) =>
  `${v.rstored}/${v.rbinding} pane=${v.rpane}${v.rfreshPending ? " fresh미저장" : ""} ` +
  `입력1=${fate[v.rfate[1]]}${shown(v, 1)} 입력2=${fate[v.rfate[2]]}${shown(v, 2)} 루틴=${v.rruns} 재시작=${v.rcrashes}`;
const shown = (v, k) => (v.rfate[k] === "failed" ? (v.rshown[k] ? "(알림 보임)" : "(알림 없음)") : "");
const labels = {
  RPersonSend: "사람 입력(또는 다시 보냄)", RLaunch: "스레드 pane cold 실행", RWarm: "살아 있는 pane에 입력",
  RAnswer: "같은 스레드에 답 게시·세션 저장", RIdleReap: "idle 회수", RClear: "/clear",
  RRoutineNext: "루틴 다음 실행이 스레드 세션을 바꿈", RRestart: "dcserver 재시작", RShowFailure: "불확실한 입력의 실패 알림 게시",
};
export const step = (action) => labels[action.replace(/\(.*$/, "")] ?? action;
