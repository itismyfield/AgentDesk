# HerdrRestartOwner

TLC reached 51 states and 127 steps. The chart draws 13 states and 20 arrows.
The chart does not draw 75 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 8 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User clear begins: bump the clear epoch: Clear began: epoch bumped; restart holds the transition
4. Close refused, unconfirmed or row moved on: Clear began: epoch bumped; restart holds the transition
5. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
6. Clear's teardown acts on the row: Close refused, unconfirmed or row moved on
7. Answer the failure, release: Failure answered; seed still to withdraw
8. Withdraw the seed: Clear gave up on the transition; selector still kept; the old session lives
The run stops in "Clear gave up on the transition; selector still kept; the old session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
