# HerdrRestart

TLC reached 14 states and 16 steps. The chart draws 9 states and 8 arrows.
The chart does not draw 8 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 2 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Stale input permission types into old pane: Old pane received a restart input

## Rules that hold
InputCustody, InputAtMostOnce, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
