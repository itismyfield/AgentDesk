# HerdrRestartOwner

TLC reached 18 states and 46 steps. The chart draws 10 states and 15 arrows.
The chart does not draw 23 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close refused, unconfirmed or row moved on: Close refused or unconfirmed
4. Answer the failure, release: A result awaits its visible answer
5. Submit an input to the still-current session: A result awaits its visible answer
6. Daemon dies and boots: No restart result; inputs settled
The run stops in "No restart result; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
