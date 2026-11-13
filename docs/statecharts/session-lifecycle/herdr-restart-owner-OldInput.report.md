# HerdrRestartOwner

TLC reached 24 states and 55 steps. The chart draws 10 states and 9 arrows.
The chart does not draw 41 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Stale permission types into the old pane: Old pane received an input during the restart

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
