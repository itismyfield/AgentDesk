# HerdrRestartOwner

TLC reached 30 states and 75 steps. The chart draws 13 states and 23 arrows.
The chart does not draw 41 steps that stay in one chart state.

## SeedAtMostOnce breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Daemon dies and boots: After boot: seed waits at the queue head
5. Recovery retires the row of a gone pane: After boot: seed waits at the queue head
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. Create the pane once: Owner turn installed Pending and creates
8. Submit the first input and bind the source: One new Ready session; inputs await the owner
9. Submit the seed again after boot: Restart seed executed twice

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
