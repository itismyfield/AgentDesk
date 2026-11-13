# HerdrRestartOwner

TLC reached 5 states and 10 steps. The chart draws 4 states and 4 arrows.
The chart does not draw 6 steps that stay in one chart state.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 3 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Show the result: Visible failure; inputs still owned
3. Submit an input to the still-current session: Visible failure; inputs settled
The run stops in "Visible failure; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, AcceptedInputResolves, ClearForgets
