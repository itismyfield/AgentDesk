# HerdrRestartOwner

TLC reached 187 states and 547 steps. The chart draws 26 states and 38 arrows.
The chart does not draw 358 steps that stay in one chart state.

## NoSeedAfterClear breaks
The shortest path has 15 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
8. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
9. Install Pending over the vacant row: Clear discarded the queue; a turn is launching
10. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; no session
11. Clear's teardown acts on the row: Exclusion released; superseded reply pending
12. Owner starts the front: register the launch: Owner turn began a launch (after a clear)
13. Install Pending over the vacant row: Owner turn installed Pending (after a clear)
14. Create the pane once: Create answered; first input not submitted (after a clear)
15. Submit the first input: The seed ran after a clear began

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
