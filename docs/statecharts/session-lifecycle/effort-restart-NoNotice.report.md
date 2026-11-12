# TmuxSession

TLC reached 142 states and 473 steps. The chart draws 11 states and 19 arrows.
The chart does not draw 333 steps that stay in one chart state.

## EffortInterruptedNoticeExactlyOnce breaks
The shortest path has 2 steps:
0. 기존 작업
1. effort 변경 요청: 설정 변경 접수
2. TUI 종료·중단 경계 확정: 진행 중 작업 중단

## EffortInterruptedEventuallyTold breaks
TLC's counterexample is in tlc-EffortInterruptedEventuallyTold.out. It has 15 steps:
0. 기존 작업
1. 보존한 옛 답 게시: 기존 작업
2. effort 변경 요청: 설정 변경 접수
3. 새 사용자 입력 보존: 설정 변경 접수
4. 새 사용자 입력 보존: 설정 변경 접수
5. TUI 종료·중단 경계 확정: 진행 중 작업 중단
6. 정확한 턴만 닫기: 중단 정산 완료
7. 재기동 준비 완료: 같은 세션 재기동
8. 안내 전송 실패: 같은 세션 재기동
9. 같은 세션·새 실행 확인: 자동 이어하기 접수 대기
10. 자동 이어하기 1회 접수: 자동 이어하기 실행
11. 작업 정상 종료: 입력 처리 가능
12. 보존한 사용자 입력 접수: 대기 사용자 입력 실행
13. 작업 정상 종료: 입력 처리 가능
14. 보존한 사용자 입력 접수: 대기 사용자 입력 실행
15. 작업 정상 종료: 입력 모두 처리
The run stops in "입력 모두 처리" forever.

## Rules that hold
EffortReplyOnce, EffortOldDebtNotLost, EffortInputNotLost, EffortInputOnce, EffortUserFifo, EffortContinuationAtMostOnce, EffortContinuationAuthorized, EffortContinuationFirst, EffortSameSession, EffortFailedKeepsCustody, NoDeadlock
