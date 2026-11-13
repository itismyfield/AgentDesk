# HerdrRestartOwner

TLC reached 222 states and 681 steps. The chart draws 35 states and 77 arrows.
The chart does not draw 424 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 17 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. User requests clear: Clear requested; epoch update pending
3. Terminate takes transition and fence: Clear requested; epoch update pending
4. Close only the captured pane: Clear requested; epoch update pending
5. Retire CAS and settle the old turn: Clear requested; epoch update pending
6. Disconnect keeping selector, release, wake: Clear requested; epoch update pending
7. Owner starts the front: register the launch: Clear requested; epoch update pending
8. Install Pending over the vacant row: Clear requested; epoch update pending
9. Observe the create epoch: Clear requested; epoch update pending
10. Reply to the command: Clear requested; epoch update pending
11. Compare live epoch and retain create permission: Clear requested; epoch may advance while an earlier grant lives
12. Complete the previously granted create: Clear requested; epoch update pending
13. Advance epoch and mark clearing, including while old I/O is granted: Clear began: epoch bumped; a turn is launching
14. Clear discards the queue and the launching input: Clear discarded the queue; a turn is launching
15. Create unknown: keep Pending, fail visibly: Clear discarded the queue; a new session lives
16. Clear drains earlier grants and tears down their exact session: Clear ended unconfirmed; selector still kept; a new session lives
17. Probe adopts the Pending pane: Clear ended unconfirmed; selector still kept; a new session lives
The run stops in "Clear ended unconfirmed; selector still kept; a new session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
