# HerdrRestart

TLC reached 112 states and 246 steps. The chart draws 16 states and 14 arrows.
The chart does not draw 98 steps that stay in one chart state.

## AAfterConfirmedEnd breaks
The shortest path has 1 step:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF was captured before confirmed old End

## SingleReadySession breaks
The shortest path has 6 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Verified resume intent or ordinary fresh launch
4. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
5. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Verified resume intent or ordinary fresh launch
6. Evidence and binding admit the successor: Restart lifecycle B Two live sessions after restart, Codex resume helper A Verified resume intent or ordinary fresh launch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, InflightSettledBeforeReady, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
