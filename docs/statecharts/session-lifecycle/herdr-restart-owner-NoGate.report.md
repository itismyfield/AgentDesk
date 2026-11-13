# HerdrRestartOwner

TLC reached 70 states and 169 steps. The chart draws 22 states and 26 arrows.
The chart does not draw 107 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Submit an input to the still-current session: Old pane received an input while the restart was open

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 17 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Permit refuses the seed with a notice: No seed admitted; restart goes on
3. Terminate takes transition and fence: Restart holds the transition and fence
4. Close only the captured pane: Exact close confirmed; retire and settle
5. Retire CAS and settle the old turn: Old execution retired; exclusion still held
6. Disconnect keeping selector, release, wake: Exclusion released; reply pending
7. Reply to the command: Visible failure; inputs still owned
8. Owner starts the front: register the launch: Owner turn began a launch
9. Install Pending over the vacant row: Owner turn installed Pending
10. Observe the create epoch: CheckEpoch observed; live BeginEffect still required
11. Compare live epoch and retain create permission: BeginEffect holds create authority
12. Complete the previously granted create: Create answered; first input not submitted
13. Observe the input epoch: CheckEpoch observed; live input authority still required
14. Compare live epoch and retain input permission: BeginEffect holds input authority
15. Complete the previously granted first input: First input submitted; receipt not committed
16. Commit the input's receipt: Receipt committed; source not bound
17. Bind the source: New session without a greeting; inputs settled
The run stops in "New session without a greeting; inputs settled" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, NoDeadlock, RequestResolves, CommandAnswers, AcceptedInputResolves, ClearForgets, ClearCompletes, EndedSeedRecovers, ClearRequestCompletes
