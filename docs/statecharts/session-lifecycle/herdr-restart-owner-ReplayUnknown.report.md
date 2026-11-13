# HerdrRestartOwner

TLC reached 53 states and 158 steps. The chart draws 27 states and 48 arrows.
The chart does not draw 79 steps that stay in one chart state.

## NoOperationSeedless breaks
The shortest path has 2 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting

## NoBlindCreate breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
9. Compare live epoch and retain create permission: BeginEffect holds create authority
10. Complete the previously granted create: Create answered; first input not submitted
11. Create again after an unknown reply: Unknown create acknowledgement was replayed; physical effect used no live permission; original 1 waiting

## SingleSession breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
9. Compare live epoch and retain create permission: BeginEffect holds create authority
10. Complete the previously granted create: Create answered; first input not submitted
11. Create again after an unknown reply: Unknown create acknowledgement was replayed; physical effect used no live permission; original 1 waiting

## EffectHasLivePermission breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
9. Compare live epoch and retain create permission: BeginEffect holds create authority
10. Complete the previously granted create: Create answered; first input not submitted
11. Create again after an unknown reply: Unknown create acknowledgement was replayed; physical effect used no live permission; original 1 waiting

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting
3. Terminate takes transition and fence: Restart holds the transition and fence; known ticket entered the seedless path; original 1 waiting
4. Close only the captured pane: Exact close confirmed; retire and settle; known ticket entered the seedless path; original 1 waiting
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held; known ticket entered the seedless path; original 1 waiting
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending; known ticket entered the seedless path; original 1 waiting
7. Reply to the command: Seed waits for the owner; known ticket entered the seedless path; original 1 waiting
8. Owner starts the front: register the launch: Owner turn began a launch; known ticket entered the seedless path; original 1 waiting
9. Install Pending over the vacant row: Owner turn installed Pending; known ticket entered the seedless path; original 1 waiting
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required; known ticket entered the seedless path; original 1 waiting
11. Compare live epoch and retain create permission: BeginEffect holds create authority; known ticket entered the seedless path; original 1 waiting
12. Complete the previously granted create: Create answered; first input not submitted; known ticket entered the seedless path; original 1 waiting
13. Create unknown: keep Pending, fail visibly: Pending without a launching turn; an input waits; known ticket entered the seedless path; original 1 waiting
14. Create again after an unknown reply: Unknown create acknowledgement was replayed; known ticket entered the seedless path; physical effect used no live permission; original 1 waiting
The run stops in "Unknown create acknowledgement was replayed; known ticket entered the seedless path; physical effect used no live permission; original 1 waiting" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
