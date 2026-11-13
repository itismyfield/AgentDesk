# HerdrRestartOwner

TLC reached 256 states and 683 steps. The chart draws 19 states and 33 arrows.
The chart does not draw 496 steps that stay in one chart state.

## NoResumeAfterClear breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User clear reads the row: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Accept an input into the owner queue: Old execution retired; exclusion still held
8. Seed gone: selector left as it was: A result awaits its visible answer
9. Owner starts an input: install Pending: A launch resumed the conversation a clear ended

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
9. Seed gone: selector left as it was: A result awaits its visible answer
10. Show the result: Clear gave up on the transition; selector still kept; an input waits
11. Owner starts an input: install Pending: A launch resumed the conversation a clear ended
12. Create the pane once: A launch resumed the conversation a clear ended
13. Submit the first input and bind the source: A launch resumed the conversation a clear ended
The run stops in "A launch resumed the conversation a clear ended" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves
