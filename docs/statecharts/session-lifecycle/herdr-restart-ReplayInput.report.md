# HerdrRestart

TLC reached 10 states and 13 steps. The chart draws 9 states and 9 arrows.
The chart does not draw 4 steps that stay in one chart state.

## InputAtMostOnce breaks
The shortest path has 9 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Close only the captured pane: Exact close confirmed; settle old inflight
3. Retire and settle only the old token: Old incarnation retired; originals still held
4. Commit new incarnation before create: Fresh incarnation Pending before create
5. Create reply is observed once: New pane created; wait for evidence and binding
6. Evidence and binding admit the successor: One new Ready session; originals await the owner
7. Submit an original to the successor once: One new Ready session; originals settled once
8. Forget an original receipt: One new Ready session; originals await the owner
9. Submit an original to the successor once: Original input executed twice

## Rules that hold
InputCustody, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
