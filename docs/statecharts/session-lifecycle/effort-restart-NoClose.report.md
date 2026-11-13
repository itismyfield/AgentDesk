# TmuxSession

TLC reached 32 states and 68 steps. The chart draws 3 states and 5 arrows.
The chart does not draw 51 steps that stay in one chart state.

## NoDeadlock breaks
The shortest path has 7 steps:
0. 원 작업·자연 완료 없음
1. 보존한 옛 답 게시: 원 작업·자연 완료 없음
2. effort 변경 요청: 변경 요청·원 작업 실행 중
3. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
4. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
5. TUI 종료·중단 경계 확정: 진행 중 작업 중단
6. 안내 전송 실패: 진행 중 작업 중단
7. 중단 안내 1회 게시: 진행 중 작업 중단

## EffortRequestResolves breaks
TLC's counterexample is in tlc-EffortRequestResolves.out. It has 7 steps:
0. 원 작업·자연 완료 없음
1. effort 변경 요청: 변경 요청·원 작업 실행 중
2. 보존한 옛 답 게시: 변경 요청·원 작업 실행 중
3. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
4. TUI 종료·중단 경계 확정: 진행 중 작업 중단
5. 새 사용자 입력 보존: 진행 중 작업 중단
6. 중단 안내 1회 게시: 진행 중 작업 중단
7. 안내 전송 실패: 진행 중 작업 중단
The run stops in "진행 중 작업 중단" forever.

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, EffortInterruptedEventuallyTold, EffortContinuationEventuallyInjected, EffortPendingEventuallyProcessed, EffortOldDebtEventuallyPosted
