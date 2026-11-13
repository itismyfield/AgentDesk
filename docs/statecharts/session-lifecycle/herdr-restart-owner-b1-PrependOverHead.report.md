# HerdrRestartOwner

TLC reached 33 states and 91 steps. The chart draws 21 states and 28 arrows.
The chart does not draw 45 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 7 steps:
0. Old session and a queued input; initial claim claimed
1. Read the fixed head and followers: Old session and a queued input; initial claim prepared
2. Commit the seed original (acceptance): Seed committed; gate holds it; initial claim prepared
3. Terminate takes transition and fence: Restart holds the transition and fence; initial claim prepared
4. Close only the captured pane: Exact close confirmed; retire and settle; initial claim prepared
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held; initial claim prepared
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending; initial claim prepared
7. Reply to the command: Seed waits for the owner; initial claim prepared
The run stops in "Seed waits for the owner; initial claim prepared" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 7 steps:
0. Old session and a queued input; initial claim claimed
1. Read the fixed head and followers: Old session and a queued input; initial claim prepared
2. Commit the seed original (acceptance): Seed committed; gate holds it; initial claim prepared
3. Terminate takes transition and fence: Restart holds the transition and fence; initial claim prepared
4. Close only the captured pane: Exact close confirmed; retire and settle; initial claim prepared
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held; initial claim prepared
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending; initial claim prepared
7. Reply to the command: Seed waits for the owner; initial claim prepared
The run stops in "Seed waits for the owner; initial claim prepared" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 7 steps:
0. Old session and a queued input; initial claim claimed
1. Read the fixed head and followers: Old session and a queued input; initial claim prepared
2. Commit the seed original (acceptance): Seed committed; gate holds it; initial claim prepared
3. Terminate takes transition and fence: Restart holds the transition and fence; initial claim prepared
4. Close only the captured pane: Exact close confirmed; retire and settle; initial claim prepared
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held; initial claim prepared
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending; initial claim prepared
7. Reply to the command: Seed waits for the owner; initial claim prepared
The run stops in "Seed waits for the owner; initial claim prepared" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, CommandAnswers, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
