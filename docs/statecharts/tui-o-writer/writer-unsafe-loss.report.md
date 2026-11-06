Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakeUnsafeLoss.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

# WriterWake

TLC reached 244 states and 617 steps. The chart draws 17 states and 26 arrows.
The chart does not draw 119 steps that stay in one chart state.
The view hides 248 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock
