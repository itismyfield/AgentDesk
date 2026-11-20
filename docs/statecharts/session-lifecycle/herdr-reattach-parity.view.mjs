export const title = "Herdr 재시작 재부착과 평소 입력 경로";

const names = {
  unique: "이름 유일·nonce 일치",
  duplicate: "같은 logical key의 이름 복수",
  other: "nonce 불일치",
};
const targets = {
  idle: "유휴 pane",
  busy: "작업 중 pane",
  dead: "종료된 pane",
  unknown: "pane identity 미확인",
};

export function state({ attach }) {
  const labels = {
    down: "dcserver 종료",
    await: "첫 원본 대기",
    queued: "같은 원본이 큐에 있음",
    identityHeld: attach.elapsed ? "기한이 지나도 같은 원본 보존" : "pane identity 재관찰 대기",
    endProved: "종료 증명",
    named: "이름으로 pane 선택",
    exact: "정밀 판정으로 pane 선택",
    attached: attach.grant === "busy" ? "watcher·진행 턴 lease·Busy grant 인수" : "유휴 pane 재부착",
    composerHeld: "평소 composer 차단",
    idleOnly: "작업 중인데 Idle을 별도 요구",
    reserved: attach.grant === "busy" ? "원본의 평소 Busy 주입 예약" : "원본의 평소 유휴 제출 예약",
    unknown: "응답 불명, receipt 대기",
    receipt: "제출 결산",
    done: "답 채널 게시",
    rejected: "증명된 거절",
    discarded: "살아 있는 pane의 원본 폐기",
  };
  const claim = attach.headlessClaim === "new" ? " (새 headless claim이 기존 턴을 막음)"
    : attach.headlessClaim === "refused" ? " (기존 lease에 headless claim 거절)" : "";
  const proof = attach.sourcePinned ? " (첫 프롬프트 경계 확인)" :
    attach.childEndpoint ? " (부모 endpoint와 child 실행 확인)" : "";
  const suffix = attach.submits > 1 ? " (중복 제출)" : claim + proof;
  return [targets[attach.initial], names[attach.name], labels[attach.phase] + suffix];
}

const actions = {
  Boot: "dcserver 부팅",
  FirstInput: "같은 원본 수락",
  ReconcileIntake: "intake reconcile 완료",
  ParityAssess: "현재 pane과 이름을 확인",
  ParityIdentityDeadline: "보류 기준 시각 경과",
  ParityPinPathless: "기존 턴의 정확한 시작 위치를 확인",
  ParityResolveChild: "검증된 부모 설정과 child 행을 대조",
  ParityObserve: "달라진 pane 증거로 같은 원본 재판정",
  ParityAttach: "위치·registry·watcher와 owner 인수",
  ParityHeadlessClaim: "같은 채널의 새 headless claim",
  ParityInputRule: "평소 입력 규칙 적용",
  ParityComposerClears: "composer가 입력 가능한 상태로 전환",
  ParitySubmit: "원본 한 번 전달",
  ParityObserveReceipt: "제출 증거 확인",
  ParityResendUnknown: "불명 receipt에서 다시 예약",
  ParityPublishReply: "채널에 답 게시",
  ParityReject: "종료 증명으로 결산",
  AttachDone: "결산 유지",
};

export function step(action) {
  return actions[action] ?? action;
}
