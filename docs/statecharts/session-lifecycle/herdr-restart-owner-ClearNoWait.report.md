# HerdrRestartOwner

TLC reached 884 states, more than the chart budget of 500, so the chart draws only the bug paths: 20 states and 25 arrows.

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
10. Clear drains earlier grants and tears down their exact session: Owner turn began a launch (after a clear); clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 not yet arrived

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
10. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
11. Clear drains earlier grants and tears down their exact session: Owner turn installed Pending (after a clear); clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 not yet arrived
12. Clear epoch moved: retire the unsent Pending: Exclusion released; superseded reply pending; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 not yet arrived
13. Accept an input into the owner queue: Exclusion released; superseded reply pending; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 waiting
14. Owner starts the front: register the launch: A launch resumed the conversation a clear ended; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 waiting

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 18 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Accept an input into the owner queue: Exclusion released; reply pending
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. User requests clear: Clear requested; epoch update pending
10. Observe the create epoch: Clear requested; epoch update pending
11. Compare live epoch and retain create permission: Clear requested; epoch may advance while an earlier grant lives
12. Advance epoch and mark clearing, including while old I/O is granted: BeginEffect holds create authority
13. Reply to the command: BeginEffect holds create authority
14. Clear discards the queue and the launching input: BeginEffect holds create authority
15. Complete the previously granted create: Clear discarded the queue; a turn is launching
16. Clear drains earlier grants and tears down their exact session: Create answered; first input not submitted (after a clear); clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 explicitly dropped
17. Clear epoch moved: do not submit: Clear ended unconfirmed; selector still kept; a new session lives; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 explicitly dropped
18. Probe adopts the Pending pane: Clear ended unconfirmed; selector still kept; a new session lives; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 explicitly dropped
The run stops in "Clear ended unconfirmed; selector still kept; a new session lives; clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 explicitly dropped" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
