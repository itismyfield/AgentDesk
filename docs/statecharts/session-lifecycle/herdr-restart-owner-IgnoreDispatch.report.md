# HerdrRestartOwner

TLC reached 14 states and 31 steps. The chart draws 7 states and 6 arrows.
The chart does not draw 21 steps that stay in one chart state.

## NoCloseUnderDispatch breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle

## NoInheritedDispatch breaks
The shortest path has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Successor inherited the old dispatch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
