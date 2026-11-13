# HerdrRestartOwner

TLC reached 217 states and 665 steps. The chart draws 26 states and 53 arrows.
The chart does not draw 432 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Reply to the command: Clear requested; epoch update pending
10. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
11. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
12. Owner drive parks until a wake: Clear discarded the queue; a turn is launching
13. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## ClearCompletes breaks
TLC's counterexample is in tlc-ClearCompletes.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Reply to the command: Clear requested; epoch update pending
10. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
11. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
12. Owner drive parks until a wake: Clear discarded the queue; a turn is launching
13. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## ClearRequestCompletes breaks
TLC's counterexample is in tlc-ClearRequestCompletes.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Reply to the command: Clear requested; epoch update pending
10. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
11. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
12. Owner drive parks until a wake: Clear discarded the queue; a turn is launching
13. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, EndedSeedRecovers
