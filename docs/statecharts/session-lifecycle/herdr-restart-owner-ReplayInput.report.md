# HerdrRestartOwner

TLC reached 29 states and 70 steps. The chart draws 15 states and 16 arrows.
The chart does not draw 42 steps that stay in one chart state.

## InputAtMostOnce breaks
The shortest path has 14 steps:
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
10. Commit the input's receipt: Receipt committed; source not bound
11. Bind the source: One new Ready session; inputs await the owner
12. Submit an input to the successor once: One new Ready session; inputs settled once
13. Forget an input receipt: One new Ready session; inputs await the owner
14. Submit an input to the successor once: Original input executed twice

## Rules that hold
InputCustody, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
