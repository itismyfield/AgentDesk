# HerdrRestartOwner

TLC reached 16 states and 35 steps. The chart draws 10 states and 11 arrows.
The chart does not draw 20 steps that stay in one chart state.

## InputAtMostOnce breaks
The shortest path has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. Create the pane once: Owner turn installed Pending and creates
8. Submit the first input and bind the source: One new Ready session; inputs await the owner
9. Submit an input to the successor once: One new Ready session; inputs settled once
10. Forget an input receipt: One new Ready session; inputs await the owner
11. Submit an input to the successor once: Original input executed twice

## Rules that hold
InputCustody, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
