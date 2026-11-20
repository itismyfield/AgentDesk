export const title = "dcserver 재시작 뒤 첫 Herdr 채널 입력";

const initial = {
  idle: "살아 있는 유휴 pane",
  busy: "살아 있는 작업 중 pane",
  dead: "종료가 확인된 pane",
  rowOnly: "실행 행만 확인됨",
};

export function state({ attach }) {
  const labels = {
    down: "dcserver 종료",
    await: "첫 메시지 대기",
    queued: "같은 원본이 큐에 있음",
    waiting: attach.target === "busy" ? "진행 중인 턴의 입력 경계 대기" :
      attach.target === "rowOnly" ? "pane 증거 대기" : "증거가 바뀌었으나 입력 보류",
    submit: attach.target === "dead" ? "제출 후보 pane이 사라짐" :
      attach.target === "rowOnly" ? "행만으로 제출 후보 선택" :
      attach.banner ? "이어 쓰기에 복원 표시·문맥이 붙음" : "같은 pane에 이어 쓰기",
    start: "종료된 실행의 후계 준비",
    resumed: attach.banner ? "새 프로세스 resume와 복원 표시" : "새 프로세스 resume 뒤 복원 표시 누락",
    done: attach.delivered === 1 ? "첫 메시지 1회 전달" :
      attach.delivered === 0 ? "첫 메시지 유실" : "첫 메시지 중복 전달",
  };
  const wrongRestore = attach.route === "existing" && attach.banner;
  return [
    initial[attach.initial],
    labels[attach.phase] + (wrongRestore && attach.phase !== "submit" ? " (잘못된 복원 표시)" : ""),
  ];
}

const actions = {
  Boot: "dcserver가 부팅됨",
  FirstInput: "첫 메시지를 같은 큐에 등록",
  ReconcileIntake: "입력 reconcile 완료",
  ReconnectSource: "독립 reader가 source를 대조",
  ChooseRoute: "현재 실행 증거로 경로를 고름",
  ReadinessChanges: "pane 또는 입력 준비 상태가 확인됨",
  LosePaneBeforeEffect: "첫 키 전에 pane이 종료됨",
  RecheckEffect: "제출 직전 재검사로 같은 원본을 보류",
  SubmitExisting: "기존 pane에 한 번 제출",
  ResumeEnded: "기존 실행을 정리하고 새 프로세스로 resume",
  SubmitResumed: "후계에 같은 원본을 한 번 제출",
  RedriveFront: "이미 결산한 원본의 재구동 요청",
  AttachDone: "결산 유지",
};

export function step(action, from, to) {
  if (action === "SubmitExisting" && to.attach.deadWrite) return "종료되거나 미확인인 pane에 제출";
  return actions[action] ?? action;
}
