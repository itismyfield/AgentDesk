# HerdrRestartOwner

TLC reached 256 states and 683 steps. The chart draws 16 states and 22 arrows.
The chart does not draw 490 steps that stay in one chart state.

## NoResumeAfterClear breaks
The shortest path has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear reads the row: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
7. Clear discards the queue with receipts: Clear finished; selector still kept; an input may still arrive
8. Accept an input into the owner queue: Clear finished; selector still kept; an input waits
9. Owner starts an input: install Pending: A launch resumed the conversation a clear ended

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Accept an input into the owner queue: Seed committed at the queue head; gate holds it
3. Terminate takes transition and fence: Restart holds the transition and fence
4. User clear reads the row: Restart holds the transition and fence
5. Close only the captured pane: Exact close confirmed; retire and settle
6. Retire CAS and settle the old turn: Old execution retired; exclusion still held
7. Disconnect keeping selector, release, wake: Exclusion released; seed waits for the owner
8. Clear discards the queue with receipts: Clear finished; selector still kept
The run stops in "Clear finished; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves
