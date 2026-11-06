Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakeActiveLoss.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

# WriterWake

TLC reached 28 states and 70 steps. The chart draws 6 states and 6 arrows.
The chart does not draw 14 steps that stay in one chart state.
The view hides 28 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock
