# HerdrRestartOwner

TLC reached 124 states and 364 steps. The chart draws 20 states and 29 arrows.
The chart does not draw 237 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Reply to the command: Owner turn began a launch
8. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Owner drive parks until a wake: Clear began: epoch bumped; a turn is launching
10. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
11. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
12. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## ClearCompletes breaks
TLC's counterexample is in tlc-ClearCompletes.out. It has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Reply to the command: Owner turn began a launch
8. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Owner drive parks until a wake: Clear began: epoch bumped; a turn is launching
10. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
11. Clear epoch moved: retire the unsent Pending: Clear began: epoch bumped; a finished launch never released its registration
12. Clear discards the queue and the launching input: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves
