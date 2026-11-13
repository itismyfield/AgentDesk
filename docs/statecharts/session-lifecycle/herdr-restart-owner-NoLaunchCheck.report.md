# HerdrRestartOwner

TLC reached 271 states and 857 steps. The chart draws 38 states and 65 arrows.
The chart does not draw 571 steps that stay in one chart state.

## EffectHasLivePermission breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
9. Compare live epoch and retain create permission: Create authority was observed but not retained
10. Complete the previously granted create: Create answered; first input not submitted; physical effect used no live permission; original 1 waiting

## NoStaleCreate breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
9. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
11. Compare live epoch and retain create permission: A new create grant ignored the clear epoch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
