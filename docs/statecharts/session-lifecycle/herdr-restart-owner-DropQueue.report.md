# HerdrRestartOwner

TLC reached 23 states and 55 steps. The chart draws 5 states and 5 arrows.
The chart does not draw 40 steps that stay in one chart state.

## InputCustody breaks
The shortest path has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Accepted input lost its owner

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Accepted input lost its owner
5. Disconnect keeping selector, release, wake: Accepted input lost its owner
6. Reply to the command: Accepted input lost its owner
7. Owner starts the front: register the launch: Accepted input lost its owner
8. Install Pending over the vacant row: Accepted input lost its owner
9. Create the pane once: Accepted input lost its owner
10. Submit the first input: Accepted input lost its owner
11. Commit the input's receipt: Accepted input lost its owner
12. Bind the source: Accepted input lost its owner
The run stops in "Accepted input lost its owner" forever.

## Rules that hold
InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, ClearForgets, ClearCompletes
