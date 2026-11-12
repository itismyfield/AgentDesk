export const title = "루틴 한 실행의 판정·입력·종결";
export const state = ({s}) => {
  if (s.sharedDestroyed) return "공유 채널 세션 종료됨";
  if (s.mode === "script" && s.acceptedTurns > 0) return "스크립트에서 에이전트 실행";
  if (s.acceptedTurns > 1) return "두 번 수용된 루틴";
  if (s.lateAcceptedTurns > 0) return "취소·종결 뒤 입력 실행";
  if (["succeeded", "failed", "interrupted", "skipped", "paused"].includes(s.status)) {
    if (!s.recorded) return "기록과 알림 없이 사라짐";
    if (s.queued) return "종결 뒤 대기 입력 남음";
    if (s.status === "succeeded" && s.outcomeKind === "agent" && s.acceptedTurns === 0) return "턴 0을 에이전트 성공으로 오인";
    if (s.outcomeKind === "command_consumed") return "명령 소비·에이전트 턴 0";
    if (s.reason === "prejudge_no_work") return "할 일 없음·세션 생략";
    return s.status === "succeeded" ? "실행 완료" : s.status === "skipped" ? "실행 건너뜀" : s.status === "paused" ? "일시정지와 알림" : s.status === "interrupted" ? "중단 기록과 알림" : "실패 기록과 알림";
  }
  if (s.recovering) return s.input === "cancelled" ? "취소 확인·중단 저장 대기" : ["MayHaveSent", "started"].includes(s.input) ? "복구 중 제출 증거 정산" : "복구 확인 대기";
  if (s.expired) return s.input === "input_pending" ? "보류 기한 종료" : s.input === "MayHaveSent" ? "기한 뒤 제출 증거 정산" : "종결 기한 종료";
  if (s.input === "none" && !s.prejudgeDone) return "에이전트 착수 사전 판정";
  if (s.input === "none" && ["rc2", "rc3"].includes(s.prejudgeResult)) return "판정 불확실·기존 규칙";
  if (s.input === "none" && s.prejudgeResult === "rc0_yes") return "할 일 있음·실행 준비";
  return ({none:"루틴 실행 시작", input_pending:"같은 입력을 보관", accepted:"제출 준비", NotSent:"제출 안 됨 확인", MayHaveSent:"제출 여부 정산", started:"한 번 수용됨", retry_scheduled:"시작한 턴을 재시도"})[s.input];
};
const labels = {
 Prejudge:"스크립트 자료를 한 번 판정", FailBeforeSubmit:"제출 전 실패를 기록", ConsumeCommand:"명령만 소비하고 종결",
 Admit:"입력을 한 번 등록", OwnerAccept:"기존 소유자가 수용", RefuseBeforeKey:"첫 키 전 거절",
 RearmKnownUnsent:"같은 입력 재시도", Submit:"기존 소유자가 프롬프트 제출", NativeAccept:"늦은 native 수용 확인", ObserveAccepted:"실제 수용 확인",
 ObserveNotSent:"미제출 증거 확인", CompleteAgent:"결과 저장", CompleteScript:"스크립트 결과 저장",
 SkipOrPause:"JS가 건너뜀 또는 일시정지", ProviderFailure:"에이전트 실패 처리", RetryStarted:"새 턴 재시도",
 ResendUnknown:"불확실 입력 재전송", Expire:"실행 기한 도달", CancelUnsentAtDeadline:"정확한 대기 입력 취소 후 종결",
 CloseUncertainAtDeadline:"재전송 없이 정산 종료", DropHeld:"보류 입력 버림", LateDispatch:"남은 입력이 뒤늦게 실행",
 CancelAfterKey:"첫 키 뒤 취소로 오인", TeardownChannel:"루틴 정리로 공유 채널 세션 종료",
 BeginRecovery:"만료된 실행 복구 시작", RecoverWithdraw:"정확한 대기 입력 취소 확인", RecoverUnsent:"중단 결과 저장",
 RecoverWithoutWithdraw:"취소 없이 중단 상태만 저장", RecoverUncertain:"보낸 입력을 재전송 없이 중단", AwaitRecoveryCommit:null,
 AwaitHeld:"보류 유지", Restart:null, Done:null
};
export const step = (action, from, to) => {
  if (action === "BeginRecovery" && !["none", "input_pending", "accepted"].includes(from.s.input)) return null;
  if (action === "CancelUnsentAtDeadline" && to.s.queued) return "취소 없이 실패 상태만 저장";
  if (action === "Prejudge") return to.s.reason === "prejudge_no_work" ? "rc0·신뢰도 충족·할 일 없음" : to.s.prejudgeResult === "rc0_yes" ? "rc0·할 일 있음" : "rc2·3 무시하고 기존 규칙";
  return Object.hasOwn(labels, action) ? labels[action] : action;
};
export const group = ({s}) => s.status === "running" ? "running" : s.status === "succeeded" ? "done" : "closed";
export const colors = {running:"blue", done:"teal", closed:"orange"};
