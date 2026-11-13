# HerdrRestartOwner

TLC reached 268 states and 792 steps. The chart draws 32 states and 58 arrows.
The chart does not draw 513 steps that stay in one chart state.

## NoSeedAfterClear breaks
The shortest path has 20 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch serializer pending
8. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
10. Install Pending over the vacant row: Clear discarded the queue; a turn is launching
11. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; no session
12. Clear's teardown acts on the row: Exclusion released; superseded reply pending
13. Owner starts the front: register the launch: Owner turn began a launch (after a clear)
14. Install Pending over the vacant row: Owner turn installed Pending (after a clear)
15. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
16. Compare live epoch and retain create permission: BeginEffect holds create authority
17. Create once under live effect permission: Create answered; first input not submitted (after a clear)
18. Observe the input epoch: CheckEpoch observed; live input authority still required
19. Compare live epoch and retain input permission: BeginEffect holds input authority
20. Submit the first input under live permission: The seed ran after a clear began

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
