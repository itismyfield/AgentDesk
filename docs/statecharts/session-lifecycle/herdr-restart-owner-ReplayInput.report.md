# HerdrRestartOwner

TLC reached 73 states and 214 steps. The chart draws 37 states and 57 arrows.
The chart does not draw 107 steps that stay in one chart state.

## NoOperationDuplicateSeed breaks
The shortest path has 2 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Insert a duplicate operation seed: Seed committed; gate holds it; known operation inserted another seed; original 1 waiting

## InputAtMostOnce breaks
The shortest path has 18 steps:
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
15. Bind the source: One new Ready session; inputs await the owner
16. Submit an input to the successor once: One new Ready session; inputs settled once
17. Forget an input receipt: One new Ready session; inputs await the owner
18. Submit an input to the successor once: Original input executed twice

## Rules that hold
InputCustody, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
