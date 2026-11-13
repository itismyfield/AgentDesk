# HerdrRestartOwner

TLC reached 402 states and 1301 steps. The chart draws 34 states and 61 arrows.
The chart does not draw 905 steps that stay in one chart state.

## NoResumeAfterClear breaks
The shortest path has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
8. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
9. Accept an input into the owner queue: Clear discarded the queue; a turn is launching
10. Install Pending over the vacant row: Clear discarded the queue; a turn is launching
11. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; no session
12. Clear's teardown acts on the row: Exclusion released; superseded reply pending
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
12. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; no session
13. Clear's teardown acts on the row: Clear ended; selector still kept
The run stops in "Clear ended; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
