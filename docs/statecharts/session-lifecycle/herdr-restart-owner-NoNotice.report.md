# HerdrRestartOwner

TLC reached 91 states and 223 steps. The chart draws 26 states and 40 arrows.
The chart does not draw 114 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Daemon dies and boots: After boot: seed waits; old session lives
3. Permit refuses the seed with a notice: Restart answered; inputs wait; old session lives
4. Submit an input to the still-current session: Restart answered; inputs settled; old session lives
The run stops in "Restart answered; inputs settled; old session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
