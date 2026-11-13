# HerdrRestartOwner

TLC reached 247 states and 775 steps. The chart draws 36 states and 58 arrows.
The chart does not draw 521 steps that stay in one chart state.

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
10. Create once under live effect permission: Create answered; first input not submitted; physical effect used no live permission; original 1 waiting

## NoStaleCreate breaks
The shortest path has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch serializer pending
8. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
11. Compare live epoch and retain create permission: Create authority was observed but not retained
12. Create once under live effect permission: A launch older than the clear created a session; physical effect used no live permission; original 1 waiting

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
