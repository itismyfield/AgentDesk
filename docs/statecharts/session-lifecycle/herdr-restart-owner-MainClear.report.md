# HerdrRestartOwner

TLC reached 364 states and 1181 steps. The chart draws 27 states and 56 arrows.
The chart does not draw 884 steps that stay in one chart state.

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
11. Compare live epoch and retain create permission: BeginEffect holds create authority
12. Create once under live effect permission: A launch older than the clear created a session

## NoSessionAfterClear breaks
The shortest path has 14 steps:
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
12. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
13. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
14. Clear's teardown acts on the row: Clear reported a hold and left the launch running

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
13. Acquire the serializer and bump the clear epoch: CheckEpoch observed; live input authority still required
14. Compare live epoch and retain input permission: BeginEffect holds input authority
15. Submit the first input under live permission: The seed ran after a clear began

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 20 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Reply to the command: Seed waits for the owner
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
10. Compare live epoch and retain create permission: BeginEffect holds create authority
11. User requests clear: Clear requested; wait for the accepted physical effect
12. Create once under live effect permission: Clear requested; epoch serializer pending
13. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
14. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
15. Clear's teardown acts on the row: Clear reported a hold and left the launch running
16. Observe the input epoch: Clear reported a hold and left the launch running
17. Compare live epoch and retain input permission: Clear reported a hold and left the launch running
18. Submit the first input under live permission: The seed ran after a clear began
19. Commit the input's receipt: The seed ran after a clear began
20. Bind the source: The seed ran after a clear began
The run stops in "The seed ran after a clear began" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
