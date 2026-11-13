# HerdrRestartOwner

TLC reached 69 states and 170 steps. The chart draws 20 states and 35 arrows.
The chart does not draw 90 steps that stay in one chart state.

## NoInheritedDispatch breaks
The shortest path has 6 steps:
0. Old session and a queued input
1. Seed not admitted: restart anyway: No seed admitted; restart goes on
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Daemon dies and boots: Restart answered; inputs wait
5. Recovery retires the row of a gone pane: Restart answered; inputs wait
6. Owner starts the front: register the launch: Successor inherited the old dispatch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
