# HerdrRestartOwner

TLC reached 155 states and 459 steps. The chart draws 22 states and 40 arrows.
The chart does not draw 329 steps that stay in one chart state.

## NoSeedAfterClear breaks
The shortest path has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Launch the successor inside the exclusion: The seed ran after a clear began

## NoSessionAfterClear breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
5. Close only the captured pane: Clear discarded the queue; restart holds the transition
6. Retire CAS and settle the old turn: Clear discarded the queue; restart holds the transition
7. Clear's teardown acts on the row: Old execution retired; a clear overtook the restart
8. Launch the successor inside the exclusion: Exclusion released; superseded reply pending

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Accept an input into the owner queue: Exact close confirmed; retire and settle
5. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
6. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
7. Clear's teardown acts on the row: Exact close confirmed; retire and settle
8. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
9. Launch the successor inside the exclusion: Exclusion released; superseded reply pending
10. Reply to the command: Clear gave up on the transition; selector still kept; a new session lives
The run stops in "Clear gave up on the transition; selector still kept; a new session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
