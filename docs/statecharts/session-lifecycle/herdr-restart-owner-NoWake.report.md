# HerdrRestartOwner

TLC reached 15 states and 32 steps. The chart draws 10 states and 10 arrows.
The chart does not draw 19 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Owner drive parks until a wake: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Seed waits; the owner drive was never woken
The run stops in "Seed waits; the owner drive was never woken" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Owner drive parks until a wake: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Seed waits; the owner drive was never woken
The run stops in "Seed waits; the owner drive was never woken" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 6 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Owner drive parks until a wake: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release without a wake: Seed waits; the owner drive was never woken
The run stops in "Seed waits; the owner drive was never woken" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RefusalVisible, ClearForgets
