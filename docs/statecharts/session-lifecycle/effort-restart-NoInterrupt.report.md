# TmuxSession

TLC reached 8 states and 10 steps. The chart draws 2 states and 3 arrows.
The chart does not draw 5 steps that stay in one chart state.

## NoDeadlock breaks
The shortest path has 4 steps:
0. 원 작업·자연 완료 없음
1. 보존한 옛 답 게시: 원 작업·자연 완료 없음
2. effort 변경 요청: 변경 요청·원 작업 실행 중
3. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
4. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중

## EffortRequestResolves breaks
TLC's counterexample is in tlc-EffortRequestResolves.out. It has 4 steps:
0. 원 작업·자연 완료 없음
1. 보존한 옛 답 게시: 원 작업·자연 완료 없음
2. effort 변경 요청: 변경 요청·원 작업 실행 중
3. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
4. 새 사용자 입력 보존: 변경 요청·원 작업 실행 중
The run stops in "변경 요청·원 작업 실행 중" forever.

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, EffortInterruptedEventuallyTold, EffortContinuationEventuallyInjected, EffortPendingEventuallyProcessed, EffortOldDebtEventuallyPosted
