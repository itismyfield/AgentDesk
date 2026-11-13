# HerdrRestartOwner

TLC reached 66 states and 162 steps. The chart draws 20 states and 30 arrows.
The chart does not draw 103 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Daemon dies and boots: After boot: seed waits; old session lives
3. Permit refuses the seed with a notice: Restart record vanished without a result
4. Submit an input to the still-current session: Restart record vanished without a result
The run stops in "Restart record vanished without a result" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
