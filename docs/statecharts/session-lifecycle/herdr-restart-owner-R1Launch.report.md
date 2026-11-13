# HerdrRestartOwner

TLC reached 273 states and 814 steps. The chart draws 63 states and 101 arrows.
The chart does not draw 492 steps that stay in one chart state.

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
3. User requests clear: Clear requested; epoch serializer pending
4. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
5. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
6. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
7. Launch the successor inside the exclusion: The seed ran after a clear began; physical effect used no live permission; original 1 waiting, original 2 not yet arrived

## NoSessionAfterClear breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User requests clear: Clear requested; epoch serializer pending
4. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
5. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
6. Close only the captured pane: Clear discarded the queue; restart holds the transition
7. Retire CAS and settle the old turn: Clear discarded the queue; restart holds the transition
8. Clear's teardown acts on the row: Old execution retired; a clear overtook the restart
9. Launch the successor inside the exclusion: Exclusion released; superseded reply pending; physical effect used no live permission; original 1 explicitly dropped, original 2 not yet arrived

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User requests clear: Clear requested; epoch serializer pending
5. Accept an input into the owner queue: Clear requested; epoch serializer pending
6. Retire CAS and settle the old turn: Clear requested; epoch serializer pending
7. Launch the successor inside the exclusion: Clear requested; epoch serializer pending; physical effect used no live permission; original 1 waiting, original 2 waiting
8. Reply to the command: Clear requested; epoch serializer pending; physical effect used no live permission; original 1 waiting, original 2 waiting
9. Submit an input to the successor once: Clear requested; epoch serializer pending; physical effect used no live permission; original 1 delivered, original 2 waiting
10. Submit an input to the successor once: Clear requested; epoch serializer pending; physical effect used no live permission; original 1 delivered, original 2 delivered
11. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a new session lives; physical effect used no live permission; original 1 delivered, original 2 delivered
12. Clear discards the queue and the launching input: Clear discarded the queue; a new session lives; physical effect used no live permission; original 1 delivered, original 2 delivered
13. Clear's teardown acts on the row: One new Ready session; inputs settled once; physical effect used no live permission; original 1 delivered, original 2 delivered
The run stops in "One new Ready session; inputs settled once; physical effect used no live permission; original 1 delivered, original 2 delivered" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
