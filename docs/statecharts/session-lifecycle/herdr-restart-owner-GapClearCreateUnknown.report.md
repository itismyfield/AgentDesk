# HerdrRestartOwner

TLC reached 200 states and 580 steps. The chart draws 32 states and 64 arrows.
The chart does not draw 366 steps that stay in one chart state.

## ClearForgets breaks
TLC's counterexample is in tlc-ClearForgets.out. It has 14 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Retire CAS and settle the old turn: Old execution retired; exclusion still held
5. Disconnect keeping selector, release, wake: Exclusion released; reply pending
6. Owner starts the front: register the launch: Owner turn began a launch
7. Reply to the command: Owner turn began a launch
8. Install Pending over the vacant row: Owner turn installed Pending
9. Create the pane once: Create answered; first input not submitted
10. User clear begins: bump the clear epoch: Clear began: epoch bumped; a turn is launching
11. Create unknown: keep Pending, fail visibly: Clear began: epoch bumped; a new session lives
12. Clear discards the queue and the launching input: Clear discarded the queue; a new session lives
13. Clear's teardown acts on the row: Clear ended unconfirmed; selector still kept; a new session lives
14. Probe adopts the Pending pane: Clear ended unconfirmed; selector still kept; a new session lives
The run stops in "Clear ended unconfirmed; selector still kept; a new session lives" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearCompletes
