# HerdrRestartOwner

TLC reached 13 states and 29 steps. The chart draws 5 states and 5 arrows.
The chart does not draw 18 steps that stay in one chart state.

## InputCustody breaks
The shortest path has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Accepted input lost its owner

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Accepted input lost its owner
5. Disconnect keeping selector, release, wake: Accepted input lost its owner
6. Owner starts the seed: install Pending: Accepted input lost its owner
7. Create the pane once: Accepted input lost its owner
8. Submit the first input and bind the source: Accepted input lost its owner
The run stops in "Accepted input lost its owner" forever.

## Rules that hold
InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, ClearForgets
