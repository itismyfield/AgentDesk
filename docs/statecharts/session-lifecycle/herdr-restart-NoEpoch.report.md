# HerdrRestart

TLC reached 8 states and 8 steps. The chart draws 7 states and 7 arrows.
The chart does not draw 1 steps that stay in one chart state.

## NoSupersededReady breaks
The shortest path has 7 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Old incarnation retired; originals still held
4. Commit new incarnation before create: Fresh incarnation Pending before create
5. User clear discards originals with terminal receipts: Fresh incarnation Pending before create
6. Create reply is observed once: New pane created; wait for evidence and binding
7. Evidence and binding admit the successor: Superseded restart published Ready

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
