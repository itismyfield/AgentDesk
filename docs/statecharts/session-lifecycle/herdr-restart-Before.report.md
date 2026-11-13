# HerdrRestart

TLC reached 3 states and 4 steps. The chart draws 3 states and 2 arrows.
The chart does not draw 2 steps that stay in one chart state.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 2 steps:
0. Old session and original queued input
1. User /restart is refused before cancellation: Failure awaiting a visible result
2. Show the failure and retain input custody: Visible failure; originals remain owned
The run stops in "Visible failure; originals remain owned" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, NoDeadlock, RequestResolves, AcceptedInputResolves
