# HerdrRestartOwner

TLC reached 45 states and 113 steps. The chart draws 10 states and 13 arrows.
The chart does not draw 73 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear reads the row: Restart holds the transition and fence
4. Clear discards the queue with receipts: Restart holds the transition and fence
5. Close refused, unconfirmed or row moved on: Close refused or unconfirmed
6. Clear's terminate times out on the transition: Close refused or unconfirmed
7. Answer the failure, release: Failure answered; seed still to withdraw
8. Withdraw the seed: Clear gave up on the transition; selector still kept
The run stops in "Clear gave up on the transition; selector still kept" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves
