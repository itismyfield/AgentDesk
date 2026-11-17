# HerdrRestart

TLC reached 56 states and 136 steps. The chart draws 13 states and 11 arrows.
The chart does not draw 55 steps that stay in one chart state.

## NoForeignClose breaks
The shortest path has 2 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Reuse a numeric target after replacement: Restart lifecycle B Reused numeric target was closed, Codex resume helper A Verified resume intent or ordinary fresh launch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
