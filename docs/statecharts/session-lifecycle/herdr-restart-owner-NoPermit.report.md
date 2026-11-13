# HerdrRestartOwner

TLC reached 23 states and 60 steps. The chart draws 11 states and 17 arrows.
The chart does not draw 33 steps that stay in one chart state.

## NoFalseReady breaks
The shortest path has 3 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Daemon dies and boots: After boot: seed waits at the queue head
3. Seed typed into the old execution: Greeting reached the execution the restart should end

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 4 steps:
0. Old session and a queued input
1. Commit the seed original (acceptance): Seed committed at the queue head; gate holds it
2. Daemon dies and boots: After boot: seed waits at the queue head
3. Seed typed into the old execution: Greeting reached the execution the restart should end
4. Submit an input to the still-current session: Greeting reached the execution the restart should end
The run stops in "Greeting reached the execution the restart should end" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RefusalVisible, RestartAvailable, AcceptedInputResolves, ClearForgets
