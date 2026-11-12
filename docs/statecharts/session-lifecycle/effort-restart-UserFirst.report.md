# TmuxSession

TLC reached 342 states and 1097 steps. The chart draws 11 states and 20 arrows.
The chart does not draw 748 steps that stay in one chart state.

## EffortContinuationFirst breaks
The shortest path has 7 steps:
0. 기존 작업
1. effort 변경 요청: 설정 변경 접수
2. 새 사용자 입력 보존: 설정 변경 접수
3. TUI 종료·중단 경계 확정: 진행 중 작업 중단
4. 정확한 턴만 닫기: 중단 정산 완료
5. 재기동 준비 완료: 같은 세션 재기동
6. 같은 세션·새 실행 확인: 자동 이어하기 접수 대기
7. 보존한 사용자 입력 접수: 대기 사용자 입력 실행

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock, EffortInterruptedEventuallyTold, EffortContinuationEventuallyInjected, EffortPendingEventuallyProcessed
