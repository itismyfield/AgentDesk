Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakePark.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

구현 소스 참조: `d563afb856870980a766a77003b55a84ea60ff62` (main `4a13d1d32e0f00d4ceaff475b95c033ba7967a8b` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterWake

TLC reached 34 states and 92 steps. The chart draws 12 states and 20 arrows.
The chart does not draw 14 steps that stay in one chart state.
The view hides 38 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock, NewSourceGetsFirstReply, WaitingInputGetsFirstReply, RetainedSpoolDelivered
