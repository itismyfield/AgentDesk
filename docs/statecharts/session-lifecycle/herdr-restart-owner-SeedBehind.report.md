# HerdrRestartOwner

TLC reached 59 states and 146 steps. The chart draws 15 states and 19 arrows.
The chart does not draw 106 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 2 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Submit an input to the still-current session: Old pane received an input while the restart was open

## Rules that hold
InputCustody, InputAtMostOnce, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoDeadlock, RequestResolves, CommandAnswers, RestartAvailable, AcceptedInputResolves, ClearForgets, ClearCompletes
