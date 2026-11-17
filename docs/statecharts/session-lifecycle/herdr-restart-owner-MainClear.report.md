# HerdrRestartOwner

TLC reached 394 states and 1291 steps. The chart draws 35 states and 78 arrows.
The chart does not draw 852 steps that stay in one chart state.

## NoSessionAfterClear breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
9. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
10. Clear drains earlier grants and tears down their exact session: Clear reported a hold and left the launch running; clear completed before an older startup or grant drained; original 1 explicitly dropped

## NoStaleCreate breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
9. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
11. Compare live epoch and retain create permission: A new create grant ignored the clear epoch

## EffectHasLivePermission breaks
The shortest path has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
9. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
11. Compare live epoch and retain create permission: A new create grant ignored the clear epoch
12. Complete the previously granted create: A new create grant ignored the clear epoch; physical effect used no live permission; original 1 waiting

## NoSeedAfterClear breaks
The shortest path has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Observe the create epoch: Clear requested; epoch update pending
10. Compare live epoch and retain create permission: Clear requested; epoch may advance while an earlier grant lives
11. Advance epoch and mark clearing, including while old I/O is granted: BeginEffect holds create authority
12. Complete the previously granted create: Clear began: epoch bumped; a turn is launching
13. Observe the input epoch: CheckEpoch observed; live input authority still required
14. Compare live epoch and retain input permission: A new seed grant was issued after clear

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 20 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User requests clear: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Observe the create epoch: Clear requested; epoch update pending
10. Reply to the command: Clear requested; epoch update pending
11. Advance epoch and mark clearing, including while old I/O is granted: CheckEpoch observed; live BeginEffect still required
12. Compare live epoch and retain create permission: A new create grant ignored the clear epoch
13. Complete the previously granted create: A new create grant ignored the clear epoch; physical effect used no live permission; original 1 waiting
14. Observe the input epoch: A new create grant ignored the clear epoch; physical effect used no live permission; original 1 waiting
15. Compare live epoch and retain input permission: A new seed grant was issued after clear; physical effect used no live permission; original 1 waiting
16. Complete the previously granted first input: A new seed grant was issued after clear; physical effect used no live permission; original 1 waiting
17. Clear discards the queue and the launching input: A new seed grant was issued after clear; physical effect used no live permission; original 1 explicitly dropped
18. Commit the input's receipt: A new seed grant was issued after clear; physical effect used no live permission; original 1 explicitly dropped
19. Clear drains earlier grants and tears down their exact session: A new seed grant was issued after clear; physical effect used no live permission; clear completed before an older startup or grant drained; original 1 explicitly dropped
20. Bind the source: A new seed grant was issued after clear; physical effect used no live permission; clear completed before an older startup or grant drained; original 1 explicitly dropped
The run stops in "A new seed grant was issued after clear; physical effect used no live permission; clear completed before an older startup or grant drained; original 1 explicitly dropped" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
