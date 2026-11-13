# HerdrRestartOwner

TLC reached 182 states and 506 steps. The chart draws 8 states and 11 arrows.
The chart does not draw 394 steps that stay in one chart state.

## NoResumeAfterClear breaks
The shortest path has 7 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. User clear reads the row: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Launch the successor inside the exclusion: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 11 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. User clear reads the row: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Clear discards the queue with receipts: Old execution retired; exclusion still held
7. Clear's terminate times out on the transition: Old execution retired; exclusion still held
8. Accept an input into the owner queue: Old execution retired; exclusion still held
9. Launch the successor inside the exclusion: A launch resumed the conversation a clear ended
10. Show the result: A launch resumed the conversation a clear ended
11. Submit an input to the successor once: A launch resumed the conversation a clear ended
The run stops in "A launch resumed the conversation a clear ended" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves
