# HerdrRestartOwner

TLC reached 282 states and 725 steps. The chart draws 13 states and 18 arrows.
The chart does not draw 572 steps that stay in one chart state.

## NoSeedRevival breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User clear reads the row: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Queue the seed again after the clear: A cleared seed was queued again

## NoResumeAfterClear breaks
The shortest path has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User clear reads the row: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Queue the seed again after the clear: A cleared seed was queued again
8. Owner starts the seed: install Pending: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 13 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User clear reads the row: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Clear's terminate times out on the transition: Old execution retired; exclusion still held
8. Accept an input into the owner queue: Old execution retired; exclusion still held
9. Queue the seed again after the clear: A cleared seed was queued again
10. Owner starts the seed: install Pending: A launch resumed the conversation a clear ended
11. Create the pane once: A launch resumed the conversation a clear ended
12. Submit the first input and bind the source: A launch resumed the conversation a clear ended
13. Submit an input to the successor once: A launch resumed the conversation a clear ended
The run stops in "A launch resumed the conversation a clear ended" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves
