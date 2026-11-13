# HerdrRestartOwner

TLC reached 193 states and 581 steps. The chart draws 26 states and 46 arrows.
The chart does not draw 378 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 16 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Reply to the command: Seed waits for the owner
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
10. Compare live epoch and retain create permission: BeginEffect holds create authority
11. User requests clear: Clear requested; wait for the accepted physical effect
12. Create once under live effect permission: Clear requested; epoch serializer pending
13. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
14. Owner drive parks until a wake: Clear began: epoch bumped; a turn is launching
15. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
16. Clear epoch moved: do not submit: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## ClearCompletes breaks
TLC's counterexample is in tlc-ClearCompletes.out. It has 16 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Reply to the command: Seed waits for the owner
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
10. Compare live epoch and retain create permission: BeginEffect holds create authority
11. User requests clear: Clear requested; wait for the accepted physical effect
12. Create once under live effect permission: Clear requested; epoch serializer pending
13. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
14. Owner drive parks until a wake: Clear began: epoch bumped; a turn is launching
15. Clear epoch moved: do not submit: Clear began: epoch bumped; a finished launch never released its registration
16. Clear discards the queue and the launching input: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## ClearRequestCompletes breaks
TLC's counterexample is in tlc-ClearRequestCompletes.out. It has 16 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Reply to the command: Seed waits for the owner
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
10. Compare live epoch and retain create permission: BeginEffect holds create authority
11. User requests clear: Clear requested; wait for the accepted physical effect
12. Create once under live effect permission: Clear requested; epoch serializer pending
13. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a turn is launching
14. Owner drive parks until a wake: Clear began: epoch bumped; a turn is launching
15. Clear epoch moved: do not submit: Clear began: epoch bumped; a finished launch never released its registration
16. Clear discards the queue and the launching input: Clear discarded the queue; a finished launch never released its registration
The run stops in "Clear discarded the queue; a finished launch never released its registration" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, EndedSeedRecovers
