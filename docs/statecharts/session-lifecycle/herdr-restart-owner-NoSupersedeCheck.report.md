# HerdrRestartOwner

TLC reached 923 states, more than the chart budget of 500, so the chart draws only the bug paths: 14 states and 17 arrows.

## NoResumeAfterClear breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch serializer pending
6. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
7. Clear epoch ignored: selector kept: Clear began: epoch bumped; no session
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
7. Clear epoch ignored: selector kept: Clear began: epoch bumped; no session
8. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
9. Install Pending over the vacant row: A launch resumed the conversation a clear ended
10. Observe the create epoch: A launch resumed the conversation a clear ended
11. Compare live epoch and retain create permission: A launch resumed the conversation a clear ended
12. Create once under live effect permission: A launch resumed the conversation a clear ended
13. Observe the input epoch: A launch resumed the conversation a clear ended
14. Compare live epoch and retain input permission: A launch resumed the conversation a clear ended
15. Submit the first input under live permission: The seed ran after a clear began

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 11 steps:
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
10. Clear epoch ignored: selector kept: Exclusion released; superseded reply pending
11. Reply to the command: Clear gave up on the transition; selector still kept
The run stops in "Clear gave up on the transition; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
