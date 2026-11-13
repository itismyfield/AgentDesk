# HerdrRestart

TLC reached 8 states and 9 steps. The chart draws 6 states and 6 arrows.
The chart does not draw 3 steps that stay in one chart state.

## NoBlindCreate breaks
The shortest path has 6 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Old incarnation retired; originals still held
4. Commit new incarnation before create: Fresh incarnation Pending before create
5. Create reply is observed once: Fresh incarnation Pending before create
6. Create reply is observed once: Unknown create acknowledgement was replayed

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
