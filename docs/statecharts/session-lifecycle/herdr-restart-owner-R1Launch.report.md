# HerdrRestartOwner

TLC reached 263 states and 799 steps. The chart draws 58 states and 92 arrows.
The chart does not draw 487 steps that stay in one chart state.

## EffectHasLivePermission breaks
The shortest path has 5 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Launch the successor inside the exclusion: One new Ready session; inputs await the owner; physical effect used no live permission; original 1 waiting, original 2 not yet arrived

## NoSeedAfterClear breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User requests clear: Clear requested; epoch update pending
4. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
5. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
6. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
7. Launch the successor inside the exclusion: A new seed grant was issued after clear; physical effect used no live permission; original 1 waiting, original 2 not yet arrived

## NoSessionAfterClear breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User requests clear: Clear requested; epoch update pending
4. Close only the captured pane: Clear requested; epoch update pending
5. Retire CAS and settle the old turn: Clear requested; epoch update pending
6. Launch the successor inside the exclusion: Clear requested; epoch update pending; physical effect used no live permission; original 1 waiting, original 2 not yet arrived
7. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a new session lives; physical effect used no live permission; original 1 waiting, original 2 not yet arrived
8. Clear discards the queue and the launching input: Clear discarded the queue; a new session lives; physical effect used no live permission; original 1 explicitly dropped, original 2 not yet arrived
9. Clear drains earlier grants and tears down their exact session: One new Ready session; inputs settled once; an input may still arrive; physical effect used no live permission; original 1 explicitly dropped, original 2 not yet arrived

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Accept an input into the owner queue: Old execution retired; exclusion still held
6. User requests clear: Clear requested; epoch update pending
7. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; restart holds the transition
8. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
9. Launch the successor inside the exclusion: Clear discarded the queue; a new session lives; physical effect used no live permission; original 1 explicitly dropped, original 2 explicitly dropped
10. Clear drains earlier grants and tears down their exact session: Exclusion released; superseded reply pending; physical effect used no live permission; original 1 explicitly dropped, original 2 explicitly dropped
11. Reply to the command: Clear ended unconfirmed; selector still kept; a new session lives; physical effect used no live permission; original 1 explicitly dropped, original 2 explicitly dropped
The run stops in "Clear ended unconfirmed; selector still kept; a new session lives; physical effect used no live permission; original 1 explicitly dropped, original 2 explicitly dropped" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
