# TmuxSession

TLC reached 448 states and 1632 steps. The chart draws 12 states and 21 arrows.
The chart does not draw 1263 steps that stay in one chart state.

## EffortReplyOnce breaks
The shortest path has 6 steps:
0. 기존 작업
1. 작업 정상 종료: 기존 작업
2. effort 변경 요청: 설정 변경 접수
3. 실행 중 작업 없음: 중단 정산 완료
4. 재기동 준비 완료: 같은 세션 재기동
5. 같은 세션·새 실행 확인: 새 입력 대기
6. 같은 답 재게시: 같은 답 중복 게시

## Rules that hold
EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock
