# HerdrRestartOwner

TLC reached 501 states, more than the chart budget of 500, so the chart draws only the bug paths: 14 states and 16 arrows.

## NoResumeAfterClear breaks
The shortest path has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
10. Clear's teardown acts on the row: Owner turn installed Pending (after a clear)
11. Clear epoch moved: retire the unsent Pending: Exclusion released; superseded reply pending
12. Accept an input into the owner queue: Exclusion released; superseded reply pending
13. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Accept an input into the owner queue: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
10. Reply to the command: Clear began: epoch bumped; a turn is launching
11. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
12. Clear's teardown acts on the row: Owner turn installed Pending (after a clear)
13. Clear epoch moved: retire the unsent Pending: Clear ended unconfirmed; selector still kept
The run stops in "Clear ended unconfirmed; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
