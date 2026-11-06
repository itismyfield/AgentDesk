Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakeUnsafeLoss.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

구현 소스 참조: `2b07eed6f3a2a4ce9292456b1483d1fd9e5ef170` (main `a2ab3f26df7059ecf13d9ae9f7fcffa418560089` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterWake

TLC reached 244 states and 617 steps. The chart draws 17 states and 26 arrows.
The chart does not draw 119 steps that stay in one chart state.
The view hides 248 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock
