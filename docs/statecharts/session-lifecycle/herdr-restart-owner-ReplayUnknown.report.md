# HerdrRestartOwner

TLC reached 15 states and 34 steps. The chart draws 8 states and 8 arrows.
The chart does not draw 20 steps that stay in one chart state.

## NoBlindCreate breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. Create the pane once: Owner turn installed Pending and creates
8. Create again after an unknown reply: Unknown create acknowledgement was replayed

## SingleSession breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. Create the pane once: Owner turn installed Pending and creates
8. Create again after an unknown reply: Unknown create acknowledgement was replayed

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
6. Owner starts the seed: install Pending: Owner turn installed Pending and creates
7. Create the pane once: Owner turn installed Pending and creates
8. Create unknown: keep Pending, fail visibly: Owner turn installed Pending and creates
9. Create again after an unknown reply: Unknown create acknowledgement was replayed
The run stops in "Unknown create acknowledgement was replayed" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, ClearForgets
