# TmuxSession

TLC reached 142 states and 517 steps. The chart draws 10 states and 16 arrows.
The chart does not draw 396 steps that stay in one chart state.

## EffortContinuationEventuallyInjected breaks
TLC's counterexample is in tlc-EffortContinuationEventuallyInjected.out. It has 10 steps:
0. 기존 작업
1. effort 변경 요청: 설정 변경 접수
2. 보존한 옛 답 게시: 설정 변경 접수
3. 새 사용자 입력 보존: 설정 변경 접수
4. 새 사용자 입력 보존: 설정 변경 접수
5. TUI 종료·중단 경계 확정: 진행 중 작업 중단
6. 중단 안내 1회 게시: 진행 중 작업 중단
7. 정확한 턴만 닫기: 중단 정산 완료
8. 재기동 준비 완료: 같은 세션 재기동
9. 같은 세션·새 실행 확인: 자동 이어하기 접수 대기
10. 안내 전송 실패: 자동 이어하기 접수 대기
The run stops in "자동 이어하기 접수 대기" forever.

## EffortPendingEventuallyProcessed breaks
TLC's counterexample is in tlc-EffortPendingEventuallyProcessed.out. It has 10 steps:
0. 기존 작업
1. effort 변경 요청: 설정 변경 접수
2. 보존한 옛 답 게시: 설정 변경 접수
3. 새 사용자 입력 보존: 설정 변경 접수
4. 새 사용자 입력 보존: 설정 변경 접수
5. TUI 종료·중단 경계 확정: 진행 중 작업 중단
6. 중단 안내 1회 게시: 진행 중 작업 중단
7. 정확한 턴만 닫기: 중단 정산 완료
8. 재기동 준비 완료: 같은 세션 재기동
9. 같은 세션·새 실행 확인: 자동 이어하기 접수 대기
10. 안내 전송 실패: 자동 이어하기 접수 대기
The run stops in "자동 이어하기 접수 대기" forever.

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortInterruptedNoticeExactlyOnce, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock, EffortInterruptedEventuallyTold
