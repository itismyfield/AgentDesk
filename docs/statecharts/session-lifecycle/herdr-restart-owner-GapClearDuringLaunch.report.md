# HerdrRestartOwner

TLC reached 44 states and 106 steps. The chart draws 13 states and 15 arrows.
The chart does not draw 72 steps that stay in one chart state.

## ClearNeverHeld breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. User clear reads the row: Owner turn installed Pending and creates
8. Clear discards the queue with receipts: Clear reported a hold: a turn was launching

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
