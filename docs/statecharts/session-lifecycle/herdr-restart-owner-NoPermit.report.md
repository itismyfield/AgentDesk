# HerdrRestartOwner

TLC reached 91 states and 222 steps. The chart draws 26 states and 39 arrows.
The chart does not draw 114 steps that stay in one chart state.

## NoFalseReady breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Daemon dies and boots: After boot: seed waits; old session lives
3. Seed typed into the old execution: Greeting reached the execution the restart should end; physical effect used no live permission; original 1 waiting

## EffectHasLivePermission breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Daemon dies and boots: After boot: seed waits; old session lives
3. Seed typed into the old execution: Greeting reached the execution the restart should end; physical effect used no live permission; original 1 waiting

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Daemon dies and boots: After boot: seed waits; old session lives
3. Seed typed into the old execution: Greeting reached the execution the restart should end; physical effect used no live permission; original 1 waiting
4. Submit an input to the still-current session: Greeting reached the execution the restart should end; physical effect used no live permission; original 1 delivered
The run stops in "Greeting reached the execution the restart should end; physical effect used no live permission; original 1 delivered" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Daemon dies and boots: Idle ended execution; the seed owner must repair the row
The run stops in "Idle ended execution; the seed owner must repair the row" forever.

## EndedSeedRecovers breaks
TLC's counterexample is in tlc-EndedSeedRecovers.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed; gate holds it
2. Terminate takes transition and fence: Restart holds the transition and fence
3. Close only the captured pane: Exact close confirmed; retire and settle
4. Daemon dies and boots: Idle ended execution; the seed owner must repair the row
The run stops in "Idle ended execution; the seed owner must repair the row" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, ReadyEvidence, NoInheritedDispatch, NoDispatchRepost, NoSeedAfterClear, NoStaleCreate, NoSessionAfterClear, NoResumeAfterClear, NoSeedRevival, NoFalseSuperseded, NoOperationSeedless, NoOperationDuplicateSeed, NoDeadlock, CommandAnswers, RestartAvailable, ClearForgets, ClearCompletes, ClearRequestCompletes
