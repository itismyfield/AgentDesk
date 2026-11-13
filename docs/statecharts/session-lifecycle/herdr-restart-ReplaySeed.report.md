# HerdrRestart

TLC reached 9 states and 10 steps. The chart draws 7 states and 7 arrows.
The chart does not draw 3 steps that stay in one chart state.

## SeedAtMostOnce breaks
The shortest path has 7 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Old incarnation retired; originals still held
4. Commit new incarnation before create: Fresh incarnation Pending before create
5. Create reply is observed once: New pane created; wait for evidence and binding
6. Evidence and binding admit the successor: New pane created; wait for evidence and binding
7. Evidence and binding admit the successor: Restart seed executed twice

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
