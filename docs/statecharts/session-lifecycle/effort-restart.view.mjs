export const title = "Effort 변경: 같은 세션을 다시 열고 중단 작업 이어하기";
export const state = ({effort:e}) => {
  if(Object.values(e.replies).some(v => v > 1)) return "같은 답 중복 게시";
  if(Object.values(e.notices).some(v => v > 1)) return "중단 안내 중복 게시";
  const waiting = Object.values(e.users).length;
  const phase = {running:"기존 작업",waiting:"다음 턴 대기","waiting-herdr":"기존 작업 종료 대기",requested:"설정 변경 접수",interrupted:"진행 중 작업 중단",stopped:"중단 정산 완료",booting:"같은 세션 재기동",failed:"재기동 실패·입력 보존"}[e.phase];
  if(e.phase === "ready") return e.active === "continuation" ? "자동 이어하기 실행" : e.active === "user" ? "대기 사용자 입력 실행" : e.pending > 0 ? (e.wake ? "자동 이어하기 접수 대기" : `자동 이어하기·깨움 없음(${e.arrived}/2)`) : waiting ? (e.wake ? "입력 처리 가능" : `입력 대기·깨움 없음(${e.arrived}/2)` ) : e.arrived === 2 ? "입력 모두 처리" : "새 입력 대기";
  return phase;
};
export const step = (a) => ({EffortRequest:"effort 변경 요청",EffortSecondRequest:"새 effort 요청",EffortInputArrives:"새 사용자 입력 보존",EffortInterrupt:"TUI 종료·중단 경계 확정",EffortNoticeFails:"안내 전송 실패",EffortTellInterrupted:"중단 안내 1회 게시",EffortCloseInterrupted:"정확한 턴만 닫기",EffortNoActiveTurn:"실행 중 작업 없음",EffortCurrentHerdrApply:"준비된 pane에 설정 적용",EffortCurrentNextTurn:"다음 새 턴 도착",EffortStopTui:"재기동 준비 완료",EffortLaunchFails:"resume 실패를 알림",EffortRetryFailed:"같은 중단 건으로 재시도",EffortResume:"같은 세션·새 실행 확인",EffortContinue:"자동 이어하기 1회 접수",EffortAdmitUser:"보존한 사용자 입력 접수",EffortFinish:"작업 정상 종료",EffortDuplicateNotice:"중복 중단 안내",EffortRepost:"같은 답 재게시",EffortDeliverOldDebt:"보존한 옛 답 게시",EffortRest:null})[a] ?? a;
