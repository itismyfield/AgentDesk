# HerdrRestartOwner

TLC reached 947 states, more than the chart budget of 500, so the chart draws only the bug paths: 12 states and 20 arrows.

## NoSeedRevival breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch update pending
6. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
7. Queue the seed again after the clear: A cleared seed was queued again

## NoResumeAfterClear breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User requests clear: Clear requested; epoch update pending
5. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
6. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
7. Clear drains earlier grants and tears down their exact session: Exact close confirmed; retire and settle
8. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
9. Queue the seed again after the clear: A cleared seed was queued again
10. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## NoSeedAfterClear breaks
The shortest path has 16 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User requests clear: Clear requested; epoch update pending
6. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
7. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
8. Queue the seed again after the clear: A cleared seed was queued again
9. Clear drains earlier grants and tears down their exact session: A cleared seed was queued again
10. Owner starts the front: register the launch: A cleared seed was queued again
11. Install Pending over the vacant row: A cleared seed was queued again
12. Observe the create epoch: A cleared seed was queued again
13. Compare live epoch and retain create permission: A cleared seed was queued again
14. Complete the previously granted create: A cleared seed was queued again
15. Observe the input epoch: A cleared seed was queued again
16. Compare live epoch and retain input permission: A new seed grant was issued after clear

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 21 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User requests clear: Clear requested; epoch update pending
5. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
6. Accept an input into the owner queue: Clear began: epoch bumped; restart holds the transition
7. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
8. Clear drains earlier grants and tears down their exact session: Exact close confirmed; retire and settle
9. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
10. Queue the seed again after the clear: A cleared seed was queued again
11. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
12. Reply to the command: A launch resumed the conversation a clear ended
13. Install Pending over the vacant row: A launch resumed the conversation a clear ended
14. Observe the create epoch: A launch resumed the conversation a clear ended
15. Compare live epoch and retain create permission: A launch resumed the conversation a clear ended
16. Complete the previously granted create: A launch resumed the conversation a clear ended
17. Observe the input epoch: A launch resumed the conversation a clear ended
18. Compare live epoch and retain input permission: A new seed grant was issued after clear
19. Complete the previously granted first input: A new seed grant was issued after clear
20. Commit the input's receipt: A new seed grant was issued after clear
21. Bind the source: A new seed grant was issued after clear
The run stops in "A new seed grant was issued after clear" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
