# HerdrRestartOwner

TLC reached 166 states and 516 steps. The chart draws 21 states and 30 arrows.
The chart does not draw 378 steps that stay in one chart state.

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

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
