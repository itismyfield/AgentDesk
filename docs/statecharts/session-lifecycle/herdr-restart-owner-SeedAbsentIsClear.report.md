# HerdrRestartOwner

TLC reached 36 states and 85 steps. The chart draws 19 states and 21 arrows.
The chart does not draw 50 steps that stay in one chart state.

## NoFalseSuperseded breaks
The shortest path has 15 steps:
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
12. Compare live epoch and retain input permission: BeginEffect holds input authority
13. Complete the previously granted first input: First input submitted; receipt not committed
14. Commit the input's receipt: Receipt committed; source not bound
15. Reply reads the seed's absence as a clear: A delivered seed was reported as cleared

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
