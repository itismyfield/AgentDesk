# HerdrRestartOwner

TLC reached 59 states and 147 steps. The chart draws 20 states and 32 arrows.
The chart does not draw 80 steps that stay in one chart state.

## SeedAtMostOnce breaks
The shortest path has 12 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Install Pending over the vacant row: Owner turn installed Pending
8. Create the pane once: Create answered; first input not submitted
9. Submit the first input: First input submitted; receipt not committed
10. Daemon dies and boots: After boot: seed waits
11. Probe adopts the Pending pane: After boot: seed waits
12. Seed joins the successor: Restart seed executed twice

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
