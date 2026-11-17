# HerdrRestartOwner

TLC reached 35 states and 83 steps. The chart draws 20 states and 21 arrows.
The chart does not draw 49 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Owner drive parks until a wake: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Exclusion released; reply pending; owner not woken
7. Reply to the command: Seed waits for the owner; owner not woken
The run stops in "Seed waits for the owner; owner not woken" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Owner drive parks until a wake: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Exclusion released; reply pending; owner not woken
7. Reply to the command: Seed waits for the owner; owner not woken
The run stops in "Seed waits for the owner; owner not woken" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Owner drive parks until a wake: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Exclusion released; reply pending; owner not woken
7. Reply to the command: Seed waits for the owner; owner not woken
The run stops in "Seed waits for the owner; owner not woken" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, CommandAnswers, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
