# HerdrRestartOwner

TLC reached 229 states and 726 steps. The chart draws 21 states and 36 arrows.
The chart does not draw 558 steps that stay in one chart state.

## NoStaleCreate breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
8. Install Pending over the vacant row: Clear began: epoch bumped; a turn is launching
9. Create the pane once: A launch older than the clear created a session

## NoSeedAfterClear breaks
The shortest path has 10 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Create the pane once: Create answered; first input not submitted
9. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
10. Submit the first input: The seed ran after a clear began

## NoSessionAfterClear breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Create the pane once: Create answered; first input not submitted
9. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
10. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
11. Clear's teardown acts on the row: Clear reported a hold and left the launch running

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 15 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Reply to the command: Owner turn began a launch
8. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
9. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
10. Install Pending over the vacant row: Clear discarded the queue; a turn is launching
11. Clear's teardown acts on the row: Clear reported a hold and left the launch running
12. Create the pane once: A launch older than the clear created a session
13. Submit the first input: The seed ran after a clear began
14. Commit the input's receipt: The seed ran after a clear began
15. Bind the source: The seed ran after a clear began
The run stops in "The seed ran after a clear began" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
