# HerdrRestartOwner

TLC reached 704 states, more than the chart budget of 500, so the chart draws only the bug paths: 17 states and 20 arrows.

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
8. User requests clear: Clear requested; epoch update pending
9. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
10. Clear epoch moved: retire the unsent Pending: Clear began: epoch bumped; no session
11. Clear discards the queue and the launching input: Clear discarded the queue; no session
12. Clear drains earlier grants and tears down their exact session: Exclusion released; superseded reply pending
13. Accept an input into the owner queue: Exclusion released; superseded reply pending
14. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Accept an input into the owner queue: Exclusion released; reply pending
7. Reply to the command: Seed waits for the owner
8. Owner starts the front: register the launch: Owner turn began a launch
9. Install Pending over the vacant row: Owner turn installed Pending
10. User requests clear: Clear requested; epoch update pending
11. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
12. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
13. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; no session
14. Clear drains earlier grants and tears down their exact session: Clear ended; selector still kept
The run stops in "Clear ended; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
