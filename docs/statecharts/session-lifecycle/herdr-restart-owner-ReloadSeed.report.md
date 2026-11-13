# HerdrRestartOwner

TLC reached 1,107 states, more than the chart budget of 500, so the chart draws only the bug paths: 12 states and 18 arrows.

## NoSeedRevival breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch serializer pending
6. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
7. Queue the seed again after the clear: A cleared seed was queued again

## NoResumeAfterClear breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch serializer pending
6. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
7. Queue the seed again after the clear: A cleared seed was queued again
8. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## NoSeedAfterClear breaks
The shortest path has 15 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch serializer pending
6. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
7. Queue the seed again after the clear: A cleared seed was queued again
8. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
9. Install Pending over the vacant row: A launch resumed the conversation a clear ended
10. Observe the create epoch: A launch resumed the conversation a clear ended
11. Compare live epoch and retain create permission: A launch resumed the conversation a clear ended
12. Create once under live effect permission: A launch resumed the conversation a clear ended
13. Observe the input epoch: A launch resumed the conversation a clear ended
14. Compare live epoch and retain input permission: A launch resumed the conversation a clear ended
15. Submit the first input under live permission: The seed ran after a clear began

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 21 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User requests clear: Clear requested; epoch serializer pending
5. Accept an input into the owner queue: Clear requested; epoch serializer pending
6. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
7. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
8. Clear's teardown acts on the row: Exact close confirmed; retire and settle
9. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
10. Queue the seed again after the clear: A cleared seed was queued again
11. Reply to the command: A cleared seed was queued again
12. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
13. Install Pending over the vacant row: A launch resumed the conversation a clear ended
14. Observe the create epoch: A launch resumed the conversation a clear ended
15. Compare live epoch and retain create permission: A launch resumed the conversation a clear ended
16. Create once under live effect permission: A launch resumed the conversation a clear ended
17. Observe the input epoch: A launch resumed the conversation a clear ended
18. Compare live epoch and retain input permission: A launch resumed the conversation a clear ended
19. Submit the first input under live permission: The seed ran after a clear began
20. Commit the input's receipt: The seed ran after a clear began
21. Bind the source: The seed ran after a clear began
The run stops in "The seed ran after a clear began" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
