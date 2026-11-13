# HerdrRestartOwner

TLC reached 381 states and 1116 steps. The chart draws 20 states and 34 arrows.
The chart does not draw 935 steps that stay in one chart state.

## NoSeedRevival breaks
The shortest path has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Queue the seed again after the clear: A cleared seed was queued again

## NoResumeAfterClear breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Queue the seed again after the clear: A cleared seed was queued again
7. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

## NoSeedAfterClear breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close only the captured pane: Clear began: epoch bumped; restart holds the transition
5. Retire CAS and settle the old turn: Clear began: epoch bumped; restart holds the transition
6. Queue the seed again after the clear: A cleared seed was queued again
7. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
8. Install Pending over the vacant row: A launch resumed the conversation a clear ended
9. Create the pane once: A launch resumed the conversation a clear ended
10. Submit the first input: The seed ran after a clear began

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 16 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Accept an input into the owner queue: Exact close confirmed; retire and settle
5. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
6. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
7. Clear's teardown acts on the row: Exact close confirmed; retire and settle
8. Retire CAS and settle the old turn: Old execution retired; a clear overtook the restart
9. Queue the seed again after the clear: A cleared seed was queued again
10. Reply to the command: A cleared seed was queued again
11. Owner starts the front: register the launch: A launch resumed the conversation a clear ended
12. Install Pending over the vacant row: A launch resumed the conversation a clear ended
13. Create the pane once: A launch resumed the conversation a clear ended
14. Submit the first input: The seed ran after a clear began
15. Commit the input's receipt: The seed ran after a clear began
16. Bind the source: The seed ran after a clear began
The run stops in "The seed ran after a clear began" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
