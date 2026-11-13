# HerdrRestartOwner

TLC reached 32 states and 72 steps. The chart draws 11 states and 13 arrows.
The chart does not draw 43 steps that stay in one chart state.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Permit refuses the seed with a notice: Seed committed at the queue head; gate holds it
3. Submit an input to the still-current session: Seed committed at the queue head; gate holds it
4. Terminate takes transition and fence: Restart holds the transition and fence
5. Close only the captured pane: Exact close confirmed; retire and settle
6. Retire CAS and settle the old turn: Old execution retired; exclusion still held
7. Disconnect keeping selector, release, wake: Visible failure; inputs settled
The run stops in "Visible failure; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, AcceptedInputResolves, ClearForgets
