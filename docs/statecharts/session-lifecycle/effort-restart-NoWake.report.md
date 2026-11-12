# TmuxSession

TLC reached 242 states and 885 steps. The chart draws 16 states and 25 arrows.
The chart does not draw 656 steps that stay in one chart state.

## EffortContinuationEventuallyInjected breaks
TLC's counterexample is in tlc-EffortContinuationEventuallyInjected.out. It has 8 steps:
0. 기존 작업
1. 보존한 옛 답 게시: 기존 작업
2. effort 변경 요청: 설정 변경 접수
3. TUI 종료·중단 경계 확정: 진행 중 작업 중단
4. 정확한 턴만 닫기: 중단 정산 완료
5. 재기동 준비 완료: 같은 세션 재기동
6. 같은 세션·새 실행 확인: 자동 이어하기·깨움 없음(0/2)
7. 중단 안내 1회 게시: 자동 이어하기·깨움 없음(0/2)
8. 안내 전송 실패: 자동 이어하기·깨움 없음(0/2)
The run stops in "자동 이어하기·깨움 없음(0/2)" forever.

## EffortPendingEventuallyProcessed breaks
TLC's counterexample is in tlc-EffortPendingEventuallyProcessed.out. It has 10 steps:
0. 기존 작업
1. 보존한 옛 답 게시: 기존 작업
2. effort 변경 요청: 설정 변경 접수
3. TUI 종료·중단 경계 확정: 진행 중 작업 중단
4. 새 사용자 입력 보존: 진행 중 작업 중단
5. 새 사용자 입력 보존: 진행 중 작업 중단
6. 중단 안내 1회 게시: 진행 중 작업 중단
7. 정확한 턴만 닫기: 중단 정산 완료
8. 재기동 준비 완료: 같은 세션 재기동
9. 같은 세션·새 실행 확인: 자동 이어하기·깨움 없음(2/2)
10. 안내 전송 실패: 자동 이어하기·깨움 없음(2/2)
The run stops in "자동 이어하기·깨움 없음(2/2)" forever.

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock, EffortInterruptedEventuallyTold
