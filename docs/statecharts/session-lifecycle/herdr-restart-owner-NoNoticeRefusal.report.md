# HerdrRestartOwner

TLC reached 3 states and 5 steps. The chart draws 2 states and 2 arrows.
The chart does not draw 3 steps that stay in one chart state.

## RefusalVisible breaks
TLC's counterexample is in tlc-RefusalVisible.out. It has 2 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Unreadable row refuses the input visibly: A result awaits its visible answer
The run stops in "A result awaits its visible answer" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 2 steps:
0. Old session and a queued input
1. Refuse before any effect: A result awaits its visible answer
2. Unreadable row refuses the input visibly: A result awaits its visible answer
The run stops in "A result awaits its visible answer" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoFalseReady, SeedAtMostOnce, NoBlindCreate, SingleSession, InflightSettledBeforeReady, NoCloseUnderDispatch, NoInheritedDispatch, NoResumeAfterClear, NoSeedRevival, NoDeadlock, RequestResolves, AcceptedInputResolves, ClearForgets
