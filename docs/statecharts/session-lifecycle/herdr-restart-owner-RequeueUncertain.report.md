# HerdrRestartOwner

TLC reached 103 states and 247 steps. The chart draws 25 states and 41 arrows.
The chart does not draw 131 steps that stay in one chart state.

## SeedAtMostOnce breaks
The shortest path has 19 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
9. Compare live epoch and retain create permission: BeginEffect holds create authority
10. Complete the previously granted create: Create answered; first input not submitted
11. Observe the input epoch: CheckEpoch observed; live input authority still required
12. Compare live epoch and retain input permission: BeginEffect holds input authority
13. Complete the previously granted first input: First input submitted; receipt not committed
14. Daemon dies and boots: After boot: seed waits
15. Probe adopts the Pending pane: After boot: seed waits
16. Claim the seed on its recovered exact successor: Create answered; first input not submitted
17. Observe the input epoch: CheckEpoch observed; live input authority still required
18. Compare live epoch and retain input permission: BeginEffect holds input authority
19. Complete the previously granted first input: Restart seed executed twice

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
