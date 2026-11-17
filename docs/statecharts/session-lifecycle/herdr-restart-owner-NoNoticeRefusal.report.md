# HerdrRestartOwner

TLC reached 3 states and 5 steps. The chart draws 2 states and 2 arrows.
The chart does not draw 3 steps that stay in one chart state.

## CommandAnswers breaks
TLC's counterexample is in tlc-CommandAnswers.out. It has 2 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Unreadable row refuses the input visibly: A result awaits its visible answer
The run stops in "A result awaits its visible answer" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 2 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Unreadable row refuses the input visibly: A result awaits its visible answer
The run stops in "A result awaits its visible answer" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
