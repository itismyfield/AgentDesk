# HerdrRestart

TLC reached 7 states and 7 steps. The chart draws 7 states and 6 arrows.
The chart does not draw 1 steps that stay in one chart state.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 6 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Old incarnation retired; originals still held
4. Commit new incarnation before create: Fresh incarnation Pending before create
5. Create reply is observed once: New pane created; wait for evidence and binding
6. Evidence and binding admit the successor: New Ready session has no input wake
The run stops in "New Ready session has no input wake" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable
