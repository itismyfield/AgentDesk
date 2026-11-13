# HerdrRestartOwner

TLC reached 189 states and 550 steps. The chart draws 54 states and 107 arrows.
The chart does not draw 254 steps that stay in one chart state.

## NoOperationSeedless breaks
The shortest path has 2 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting

## NoInheritedDispatch breaks
The shortest path has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Daemon dies and boots: After boot: seed waits
6. Owner starts the front: register the launch: Successor inherited the old dispatch

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 5 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting
3. Terminate takes transition and fence: Restart holds the transition and fence; known ticket entered the seedless path; original 1 waiting
4. Close only the captured pane: Exact close confirmed; retire and settle; known ticket entered the seedless path; original 1 waiting
5. Daemon dies and boots: Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting
The run stops in "Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 5 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting
3. Terminate takes transition and fence: Restart holds the transition and fence; known ticket entered the seedless path; original 1 waiting
4. Close only the captured pane: Exact close confirmed; retire and settle; known ticket entered the seedless path; original 1 waiting
5. Daemon dies and boots: Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting
The run stops in "Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting" forever.

## EndedSeedRecovers breaks
TLC's counterexample is in tlc-EndedSeedRecovers.out. It has 5 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Daemon dies and boots: Idle ended execution; the seed owner must repair the row
5. Treat known operation as seedless capacity failure: Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting
The run stops in "Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, CommandAnswers, RestartAvailable, ClearForgets, ClearCompletes, ClearRequestCompletes
