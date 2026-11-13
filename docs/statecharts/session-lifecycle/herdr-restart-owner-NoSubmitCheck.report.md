# HerdrRestartOwner

TLC reached 241 states and 740 steps. The chart draws 37 states and 60 arrows.
The chart does not draw 501 steps that stay in one chart state.

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
10. Create once under live effect permission: Create answered; first input not submitted
11. Observe the input epoch: CheckEpoch observed; live input authority still required
12. Compare live epoch and retain input permission: Input authority was released before the effect
13. Submit the first input under live permission: First input submitted; receipt not committed; physical effect used no live permission; original 1 waiting

## NoSeedAfterClear breaks
The shortest path has 15 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch serializer pending
8. Install Pending over the vacant row: Clear requested; epoch serializer pending
9. Observe the create epoch: Clear requested; epoch serializer pending
10. Compare live epoch and retain create permission: Clear requested; wait for the accepted physical effect
11. Create once under live effect permission: Clear requested; epoch serializer pending
12. Observe the input epoch: Clear requested; epoch serializer pending
13. Compare live epoch and retain input permission: Clear requested; epoch serializer pending
14. Acquire the serializer and bump the clear epoch: Input authority was released before the effect
15. Submit the first input under live permission: The seed ran after a clear began; physical effect used no live permission; original 1 waiting

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
