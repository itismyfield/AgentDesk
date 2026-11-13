# HerdrRestartOwner

TLC reached 250 states and 745 steps. The chart draws 36 states and 81 arrows.
The chart does not draw 464 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 17 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. User requests clear: Clear requested; epoch serializer pending
4. Close only the captured pane: Clear requested; epoch serializer pending
5. Retire CAS and settle the old turn: Clear requested; epoch serializer pending
6. Disconnect keeping selector, release, wake: Clear requested; epoch serializer pending
7. Reply to the command: Clear requested; epoch serializer pending
8. Owner starts the front: register the launch: Clear requested; epoch serializer pending
9. Install Pending over the vacant row: Clear requested; epoch serializer pending
10. Observe the create epoch: Clear requested; epoch serializer pending
11. Compare live epoch and retain create permission: Clear requested; wait for the accepted physical effect
12. Create once under live effect permission: Clear requested; epoch serializer pending
13. Create unknown: keep Pending, fail visibly: Clear requested; epoch serializer pending
14. Acquire the serializer and bump the clear epoch: Clear began: epoch bumped; a new session lives
15. Clear discards the queue and the launching input: Clear discarded the queue; a new session lives
16. Clear's teardown acts on the row: Clear ended unconfirmed; selector still kept; a new session lives
17. Probe adopts the Pending pane: Clear ended unconfirmed; selector still kept; a new session lives
The run stops in "Clear ended unconfirmed; selector still kept; a new session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
