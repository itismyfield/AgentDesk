# HerdrRestartOwner

TLC reached 21 states and 58 steps. The chart draws 7 states and 7 arrows.
The chart does not draw 38 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 7 steps:
0. Old session; an input is in flight to it
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Accept an input into the owner queue: Seed committed; gate holds it
3. Terminate takes transition and fence: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Reply to the command: Seed waits for the owner
The run stops in "Seed waits for the owner" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 7 steps:
0. Old session; an input is in flight to it
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Accept an input into the owner queue: Seed committed; gate holds it
3. Terminate takes transition and fence: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Reply to the command: Seed waits for the owner
The run stops in "Seed waits for the owner" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 7 steps:
0. Old session; an input is in flight to it
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Accept an input into the owner queue: Seed committed; gate holds it
3. Terminate takes transition and fence: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Reply to the command: Seed waits for the owner
The run stops in "Seed waits for the owner" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, CommandAnswers, ClearForgets, ClearCompletes
