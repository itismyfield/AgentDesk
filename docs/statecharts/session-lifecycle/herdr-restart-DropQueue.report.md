# HerdrRestart

TLC reached 7 states and 7 steps. The chart draws 4 states and 4 arrows.
The chart does not draw 1 steps that stay in one chart state.

## InputCustody breaks
The shortest path has 3 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Accepted input lost its owner

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 6 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Accepted input lost its owner
4. Commit new incarnation before create: Accepted input lost its owner
5. Create reply is observed once: Accepted input lost its owner
6. Evidence and binding admit the successor: Accepted input lost its owner
The run stops in "Accepted input lost its owner" forever.

## Rules that hold
InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable
