# HerdrRestart

TLC reached 64 states and 136 steps. The chart draws 10 states and 8 arrows.
The chart does not draw 72 steps that stay in one chart state.

## AOrdinaryFresh breaks
The shortest path has 3 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Ordinary launch keeps fresh mode: Restart lifecycle B Old session and original queued input, Codex resume helper A Ordinary launch has no old reader boundary
2. Ordinary fresh launch keeps reader offset zero: Restart lifecycle B Old session and original queued input, Codex resume helper A Prepare an ordinary fresh incarnation with offset zero
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Ordinary launch acquired resume mode

## FreshIncarnation breaks
The shortest path has 5 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Verified resume intent or ordinary fresh launch
4. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
5. Create reply is observed once: Restart lifecycle B New pane reused the old incarnation, Codex resume helper A Verified resume intent or ordinary fresh launch

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
