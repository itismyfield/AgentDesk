# TmuxSession

TLC reached 334 states and 1329 steps. The chart draws 12 states and 26 arrows.
The chart does not draw 1004 steps that stay in one chart state.

## EffortInterruptedNoticeExactlyOnce breaks
The shortest path has 4 steps:
0. 기존 작업
1. effort 변경 요청: 설정 변경 접수
2. TUI 종료·중단 경계 확정: 진행 중 작업 중단
3. 중단 안내 1회 게시: 진행 중 작업 중단
4. 중복 중단 안내: 중단 안내 중복 게시

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock
