# HerdrRestartOwner

TLC reached 45 states and 110 steps. The chart draws 15 states and 15 arrows.
The chart does not draw 84 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Stale permission types into the old pane: Old pane received an input while the restart was open

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
