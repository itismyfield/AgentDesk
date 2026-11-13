# HerdrRestartOwner

TLC reached 27 states and 68 steps. The chart draws 11 states and 16 arrows.
The chart does not draw 40 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Daemon dies and boots: After boot: seed waits at the queue head
3. Permit refuses the seed with a notice: Restart record vanished without a result
4. Submit an input to the still-current session: Restart record vanished without a result
The run stops in "Restart record vanished without a result" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
