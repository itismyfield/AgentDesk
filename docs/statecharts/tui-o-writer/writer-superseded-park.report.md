Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakeSupersededPark.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

# WriterWake

TLC reached 14 states and 36 steps. The chart draws 6 states and 7 arrows.
The chart does not draw 4 steps that stay in one chart state.
The view hides 18 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock, NewSourceGetsFirstReply, WaitingInputGetsFirstReply, RetainedSpoolDelivered
