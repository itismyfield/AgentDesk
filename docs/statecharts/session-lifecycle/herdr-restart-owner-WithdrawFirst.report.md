# HerdrRestartOwner

TLC reached 18 states and 46 steps. The chart draws 10 states and 14 arrows.
The chart does not draw 24 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close refused, unconfirmed or row moved on: Close refused, unconfirmed or row moved on
4. Answer the failure, release: A result awaits its visible answer
5. Daemon dies and boots: Restart answered; inputs wait; old session lives
6. Submit an input to the still-current session: Restart answered; inputs settled; old session lives
The run stops in "Restart answered; inputs settled; old session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
