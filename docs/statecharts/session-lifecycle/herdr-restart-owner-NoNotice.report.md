# HerdrRestartOwner

TLC reached 28 states and 70 steps. The chart draws 12 states and 19 arrows.
The chart does not draw 40 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Daemon dies and boots: After boot: seed waits at the queue head
3. Permit refuses the seed with a notice: No restart result yet
4. Submit an input to the still-current session: No restart result; inputs settled
The run stops in "No restart result; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
