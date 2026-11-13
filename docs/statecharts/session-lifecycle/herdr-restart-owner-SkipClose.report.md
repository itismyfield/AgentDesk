# HerdrRestartOwner

TLC reached 33 states and 80 steps. The chart draws 12 states and 12 arrows.
The chart does not draw 60 steps that stay in one chart state.

## SingleSession breaks
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
9. Compare live epoch and retain create permission: BeginEffect holds create authority
10. Complete the previously granted create: Two live sessions

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
