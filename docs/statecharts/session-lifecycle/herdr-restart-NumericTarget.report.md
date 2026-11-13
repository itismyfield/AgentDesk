# HerdrRestart

TLC reached 7 states and 10 steps. The chart draws 5 states and 4 arrows.
The chart does not draw 6 steps that stay in one chart state.

## NoForeignClose breaks
The shortest path has 2 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Reuse a numeric target after replacement: Reused numeric target was closed

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves
