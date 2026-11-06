Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. Config: `WriterWakePark.cfg`; `ParkRule=TRUE` checks the #7845 park-and-run contract.

# WriterWake

TLC reached 34 states and 92 steps. The chart draws 12 states and 20 arrows.
The chart does not draw 14 steps that stay in one chart state.
The view hides 38 steps, so the chart does not draw them.

## Rules that hold
NoQuietReboot, ParkOnlySafe, ActiveLossVisible, ReadyHasNoStaleHalt, FirstReplyOnce, FirstReplyNeedsAdmission, ParkedKeepsHost, NoDeadlock, NewSourceGetsFirstReply, WaitingInputGetsFirstReply, RetainedSpoolDelivered
