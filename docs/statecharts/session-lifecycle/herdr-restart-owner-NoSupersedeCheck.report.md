# HerdrRestartOwner

TLC reached 799 states, more than the chart budget of 500, so the chart draws only the bug paths: 11 states and 13 arrows.

## NoResumeAfterClear breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User requests clear: Clear requested; epoch update pending
5. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
6. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
7. Clear drains earlier grants and tears down their exact session: Exact close confirmed; retire and settle
8. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
9. Clear epoch ignored: selector kept: Exclusion released; superseded reply pending
10. Accept an input into the owner queue: Exclusion released; superseded reply pending
11. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 11 steps:
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
10. Clear epoch ignored: selector kept: Exclusion released; superseded reply pending
11. Reply to the command: Clear gave up on the transition; selector still kept
The run stops in "Clear gave up on the transition; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
