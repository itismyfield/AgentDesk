# HerdrRestartOwner

TLC reached 287 states and 847 steps. The chart draws 34 states and 56 arrows.
The chart does not draw 668 steps that stay in one chart state.

## NoResumeAfterClear breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Clear epoch ignored: selector kept: Clear began: epoch bumped; no session
7. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## NoSeedAfterClear breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Clear epoch ignored: selector kept: Clear began: epoch bumped; no session
7. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
8. Install Pending over the vacant row: A launch resumed the conversation a clear ended
9. Create the pane once: A launch resumed the conversation a clear ended
10. Submit the first input: The seed ran after a clear began

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
9. Clear epoch ignored: selector kept: Exclusion released; superseded reply pending
10. Reply to the command: Clear gave up on the transition; selector still kept
The run stops in "Clear gave up on the transition; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
