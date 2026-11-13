# HerdrRestart

TLC reached 3 states and 3 steps. The chart draws 3 states and 2 arrows.
The chart does not draw 1 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 2 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Exact close is unconfirmed: Failure awaiting a visible result
The run stops in "Failure awaiting a visible result" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 2 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Exact close is unconfirmed: Failure awaiting a visible result
The run stops in "Failure awaiting a visible result" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 2 steps:
0. Old session and original queued input
1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane
2. Exact close is unconfirmed: Failure awaiting a visible result
The run stops in "Failure awaiting a visible result" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock
