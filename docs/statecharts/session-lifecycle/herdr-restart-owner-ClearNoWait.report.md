# HerdrRestartOwner

TLC reached 761 states, more than the chart budget of 500, so the chart draws only the bug paths: 17 states and 21 arrows.

## NoResumeAfterClear breaks
The shortest path has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. User requests clear: Clear requested; epoch serializer pending
9. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
10. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
11. Clear's teardown acts on the row: Owner turn installed Pending (after a clear)
12. Clear epoch moved: retire the unsent Pending: Exclusion released; superseded reply pending
13. Accept an input into the owner queue: Exclusion released; superseded reply pending
14. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 21 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Accept an input into the owner queue: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Owner starts the front: register the launch: Owner turn began a launch
8. User requests clear: Clear requested; epoch serializer pending
9. Install Pending over the vacant row: Clear requested; epoch serializer pending
10. Observe the create epoch: Clear requested; epoch serializer pending
11. Reply to the command: Clear requested; epoch serializer pending
12. Compare live epoch and retain create permission: Clear requested; wait for the accepted physical effect
13. Create once under live effect permission: Clear requested; epoch serializer pending
14. Observe the input epoch: Clear requested; epoch serializer pending
15. Compare live epoch and retain input permission: Clear requested; wait for the accepted physical effect
16. Submit the first input under live permission: Clear requested; epoch serializer pending
17. Commit the input's receipt: Clear requested; epoch serializer pending
18. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
19. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
20. Clear's teardown acts on the row: Receipt committed; source not bound (after a clear)
21. Bind the source: One new Ready session; inputs settled once
The run stops in "One new Ready session; inputs settled once" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
