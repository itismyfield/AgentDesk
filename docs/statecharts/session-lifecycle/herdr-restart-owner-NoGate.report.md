# HerdrRestartOwner

TLC reached 70 states and 169 steps. The chart draws 22 states and 27 arrows.
The chart does not draw 102 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Submit an input to the still-current session: Old pane received an input while the restart was open

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Submit an input to the still-current session: Old pane received an input while the restart was open
4. Terminate takes transition and fence: Old pane received an input while the restart was open
5. Close only the captured pane: Old pane received an input while the restart was open
6. Retire CAS and settle the old turn: Old pane received an input while the restart was open
7. Disconnect keeping selector, release, wake: Old pane received an input while the restart was open
8. Reply to the command: Old pane received an input while the restart was open
The run stops in "Old pane received an input while the restart was open" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
