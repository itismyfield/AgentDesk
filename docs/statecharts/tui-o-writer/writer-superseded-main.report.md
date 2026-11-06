Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakeSupersededMain.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

구현 소스 참조: `d563afb856870980a766a77003b55a84ea60ff62` (main `4a13d1d32e0f00d4ceaff475b95c033ba7967a8b` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterWake

TLC reached 7 states and 14 steps. The chart draws 6 states and 6 arrows.
The view hides 7 steps, so the chart does not draw them.

## NewSourceGetsFirstReply breaks
TLC's counterexample is in tlc-NewSourceGetsFirstReply.out. It has 6 steps:
0. 최초 실행 · 원본 재열기
1. 안전 조건 불충족 → 기존 halt: 소실로 중단
2. 기록기 종료·자리 반환: 기록기 종료
3. 주기 정리가 기록기 다시 요청: 최초 실행 · 원본 재열기 · 반복 부팅
4. 안전 조건 불충족 → 기존 halt: 소실로 중단 · 반복 부팅
5. 기록기 종료·자리 반환: 기록기 종료 · 반복 부팅
6. 주기 정리가 기록기 다시 요청: 최초 실행 · 원본 재열기 · 반복 부팅
The run loops through "최초 실행 · 원본 재열기 · 반복 부팅", "소실로 중단 · 반복 부팅" and "기록기 종료 · 반복 부팅" forever.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock, WaitingInputGetsFirstReply, RetainedSpoolDelivered
