# HerdrRestartOwner

TLC reached 271 states and 839 steps. The chart draws 38 states and 66 arrows.
The chart does not draw 570 steps that stay in one chart state.

## EffectHasLivePermission breaks
The shortest path has 13 steps:
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
11. Observe the input epoch: CheckEpoch observed; live input authority still required
12. Compare live epoch and retain input permission: Input authority was released before the effect
13. Complete the previously granted first input: First input submitted; receipt not committed; physical effect used no live permission; original 1 waiting

## NoSeedAfterClear breaks
The shortest path has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Observe the create epoch: Clear requested; epoch update pending
10. Compare live epoch and retain create permission: Clear requested; epoch may advance while an earlier grant lives
11. Advance epoch and mark clearing, including while old I/O is granted: BeginEffect holds create authority
12. Complete the previously granted create: Clear began: epoch bumped; a turn is launching
13. Observe the input epoch: CheckEpoch observed; live input authority still required
14. Compare live epoch and retain input permission: A new seed grant was issued after clear

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
