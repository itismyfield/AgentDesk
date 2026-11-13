# HerdrRestartOwner

TLC reached 54 states and 129 steps. The chart draws 18 states and 23 arrows.
The chart does not draw 82 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Submit an input to the still-current session: Old pane received an input while the restart was open

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Terminate takes transition and fence: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Owner starts the front: register the launch: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Reply to the command: Owner turn installed Pending
10. Create the pane once: Create answered; first input not submitted
11. Submit the first input: First input submitted; receipt not committed
12. Commit the input's receipt: Receipt committed; source not bound
13. Bind the source: New session without a greeting; inputs settled
The run stops in "New session without a greeting; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, AcceptedInputResolves, ClearForgets, ClearCompletes
