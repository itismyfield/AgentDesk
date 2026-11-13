# HerdrRestartOwner

TLC reached 92 states and 249 steps. The chart draws 14 states and 22 arrows.
The chart does not draw 156 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 9 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User requests clear: Clear requested; epoch serializer pending
4. Close refused, unconfirmed or row moved on: Clear requested; epoch serializer pending
5. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; restart holds the transition
6. Clear discards the queue and the launching input: Clear discarded the queue; restart holds the transition
7. Clear's teardown acts on the row: Close refused, unconfirmed or row moved on
8. Answer the failure, release: Failure answered; seed still to withdraw
9. Withdraw the seed: Clear gave up on the transition; selector still kept; the old session lives
The run stops in "Clear gave up on the transition; selector still kept; the old session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
