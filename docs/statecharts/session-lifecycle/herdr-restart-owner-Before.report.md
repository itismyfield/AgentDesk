# HerdrRestartOwner

TLC reached 5 states and 10 steps. The chart draws 4 states and 4 arrows.
The chart does not draw 6 steps that stay in one chart state.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 3 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Show the result: Visible failure; inputs still owned; old session lives
3. Submit an input to the still-current session: Visible failure; inputs settled; old session lives
The run stops in "Visible failure; inputs settled; old session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
